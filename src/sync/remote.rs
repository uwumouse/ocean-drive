/* Contains all the logic about handling updates from the remote drive, uploading and downloading files
    from remote to local
*/
use crate::auth;
use crate::google_drive::{errors::DriveError, types::File, Client};
use crate::setup::Config;
use crate::sync::util;
use crate::sync::versions::{Version, Versions};
use anyhow::{bail, Result};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
};

#[derive(Clone)]
pub struct RemoteDaemon {
    client_ref: Arc<Mutex<Client>>,
    config: Config,
    remote_dir_id: String,
    versions_ref: Arc<Mutex<Versions>>,
}

impl RemoteDaemon {
    pub fn new(
        config: Config,
        client_ref: Arc<Mutex<Client>>,
        versions_ref: Arc<Mutex<Versions>>,
        remote_dir_id: String,
    ) -> Result<Self> {
        Ok(Self {
            versions_ref,
            client_ref,
            config,
            remote_dir_id,
        })
    }

    pub fn start_sync_loop(&mut self) -> Result<()> {
        loop {
            match self.sync() {
                Ok(success) => {
                    if !success { continue }
                },
                Err(e) => bail!(e),
            }
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    }

    /// Returns wether process was succseffull of there was some issues that was handled, but
    /// synchronization wasn't finished
    pub fn sync(&self) -> Result<bool> {
        let mut client = util::lock_ref_when_free(&self.client_ref);
        let mut versions = util::lock_ref_when_free(&self.versions_ref);
        let mut versions_list = versions.list().unwrap();

        match self.sync_dir(
            &self.remote_dir_id,
            PathBuf::from_str(&self.config.local_dir).unwrap(),
            &client,
            &mut versions_list,
        ) {
            Ok(_) => {}
            Err(e) => {
                if let Some(err) = e.downcast_ref::<DriveError>() {
                    match err {
                        DriveError::Unauthorized => {
                            match auth::util::update_for_shared_client(&mut client) {
                                Ok(_) => {
                                    println!("Info: Client authorization was updated since it was out of date.");
                                    drop(client);
                                    drop(versions);
                                    return Ok(false);
                                }
                                Err(err) => bail!(err),
                            }
                        }
                        _ => {}
                    }
                }

                bail!("Unable to get updates from remote.\nDetails: {}", e);
            }
        }

        versions.save(versions_list).unwrap();
        // Make shared references avaliable again
        drop(versions);
        drop(client);

        Ok(true)
    }

    fn sync_dir(
        &self,
        id: &String,
        dir_path: PathBuf,
        client: &MutexGuard<Client>,
        local_versions: &mut HashMap<String, Version>,
    ) -> Result<()> {
        let dir_info = client.get_file(&id)?;

        if dir_info.is_none() {
            println!(
                "Warn: Unable to find directory with id '{}' in your drive. Skipping it",
                &id
            );
            return Ok(());
        }

        let dir_info = dir_info.unwrap();
        let local_dir_info = local_versions.get(id);

        // if the dir wasnt updated, then there's no need to even check this dir
        if local_dir_info.is_some() && local_dir_info.unwrap().version == dir_info.version.unwrap()
        {
            return Ok(());
        }

        let dir = client.list_files(Some(&format!("'{}' in parents", &id)), None)?;

        // Files is a haspmap with key of file id and value is file
        let mut files: HashMap<String, File> = HashMap::new();
        dir.files.iter().for_each(|f| {
            files.insert(f.id.as_ref().unwrap().clone(), f.clone());
        });

        for (file_id, file) in files.clone() {
            let is_folder =
                file.mime_type.as_ref().unwrap() == "application/vnd.google-apps.folder";
            let v = local_versions.clone();
            let local = v.get(&file_id);

            if local.is_some() {
                let local_path = Path::new(&local.unwrap().path);

                if !local_path.starts_with(&dir_path) {
                    let updated_path = dir_path.join(file.name.as_ref().unwrap());
                    let mut updated_version = local.unwrap().clone();
                    updated_version.path = updated_path.into_os_string().into_string().unwrap();
                    local_versions.remove(&file_id);
                    local_versions.insert(file_id.clone(), updated_version);
                }
            }

            // This file is new or changed
            if local.is_none() || &local.unwrap().version != file.version.as_ref().unwrap() {
                let name = &file.name.as_ref().unwrap();
                let f = dir_path.join(name).to_path_buf();
                let file_path = f.to_str().unwrap();

                if file.trashed.unwrap() {
                    local_versions.remove(&file_id);
                    self.remove_from_fs(&local)?;
                    continue;
                }

                if name.contains("/") {
                    return Ok(());
                }

                // If changed we need to update existing one. We need to remove existing for it
                if is_folder {
                    // Check directory name was changed, then just rename in on the file system
                    if let Some(local) = local {
                        if &local.path != file_path {
                            match fs::rename(&local.path, file_path) {
                                Err(e) => bail!(
                                    "Failed to rename file {:?} to {:?}: {}",
                                    local.path,
                                    file_path,
                                    e
                                ),
                                Ok(_) => {}
                            }
                        }
                    }

                    // Generate a path for a subdirectory
                    let subdir = dir_path.join(name);
                    if !subdir.exists() {
                        fs::create_dir(subdir.clone())?;
                    }

                    // We go recursively for every file in the subdir
                    self.sync_dir(&file_id, subdir, client, local_versions)?;
                } else {
                    // Check if it's a new file and download it
                    // Also re-download if we the file data has changed
                    if local.is_none() || local.unwrap().md5 != file.md5 {
                        let filepath = dir_path.join(&name);
                        self.save_file(client, &file, filepath)?;
                    }

                    // If the file is present, we check if it's was renamed
                    if let Some(local) = local {
                        if &local.path != file_path {
                            fs::rename(&local.path, &file_path)?;
                        }
                    }
                }

                // If local version is present, we need to remove it before updating
                if local.is_some() {
                    local_versions.remove(&file_id);
                }

                let latest = Version {
                    is_folder,
                    md5: file.md5,
                    parent_id: id.clone(),
                    path: dir_path.join(name).into_os_string().into_string().unwrap(),
                    version: file.version.as_ref().unwrap().to_string(),
                };
                local_versions.insert(file_id, latest.clone());
            }
        }

        Ok(())
    }

    fn save_file(
        &self,
        client: &MutexGuard<Client>,
        file: &File,
        file_path: PathBuf,
    ) -> Result<()> {
        let contents = client.download_file(file.id.as_ref().unwrap()).unwrap();

        match fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file_path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write(&contents) {
                    bail!("Error writing to file {:?}: {}", file_path.display(), e)
                }

                Ok(())
            }
            Err(e) => bail!(
                "Unable to access file {:?}: {}",
                file_path.into_os_string().into_string().unwrap(),
                e
            ),
        }
    }

    /* Removes a file from a local root, the opposite of save_file fn */
    fn remove_from_fs(&self, local: &Option<&Version>) -> Result<()> {
        if let Some(local) = local {
            let removed_path = Path::new(&local.path);

            if removed_path.exists() {
                if local.is_folder {
                    fs::remove_dir_all(&removed_path)?;
                } else {
                    fs::remove_file(&removed_path)?;
                }
            }
        }

        Ok(())
    }
}
