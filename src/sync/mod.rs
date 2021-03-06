mod local;
pub mod remote;
mod util;
mod versions;
use crate::tray::Tray;
use crate::{
    auth::{util::update_for_shared_client, Creds},
    files,
    google_drive::{errors::DriveError, types::File, Client, Session},
    setup::Config as AppConfig,
    user,
};
use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use versions::Versions;
/*
    Setups two daemons for updates: local and remote.
    Each of them is responsible for either downloading files from the remote, or uploading local files to the remote
    Each of daemons will be in the own thread.
    Threads will share a mutable referce to drive client, this will allow to keep the same authroziation
    while app is running.
*/
pub fn run() -> Result<()> {
    let conf_dir = user::get_home()?.join(".config/ocean-drive");
    let conf_file = conf_dir.join("config.toml");
    let config = files::read_toml::<AppConfig>(conf_file)?;

    let mut client = Arc::new(Mutex::new(setup_client(&conf_dir)?));
    // Get info about root dir in the drive (We do this here because daemons will need the same
    // info)
    let remote_dir = get_remote_dir(&config.drive.dir, &mut client)?;
    let versions = Arc::new(Mutex::new(Versions::new(conf_dir.join("versions.json"))?));

    let mut threads = vec![];
    // Start 2 threads for remote and local daemons
    for i in 1..=3 {
        let cl = Arc::clone(&client);
        let v = Arc::clone(&versions);
        let c = config.clone();
        let rdir_id = remote_dir.id.clone().unwrap();

        let name = match i {
            1 => "remote",
            2 => "local",
            _ => "tray",
        }
        .to_string();

        let daemon = thread::Builder::new()
            .name(name)
            .spawn(move || -> Result<()> {
                if i == 1 {
                    let mut d =
                        remote::RemoteDaemon::new(c.clone(), cl.clone(), v, rdir_id.clone())?;

                    d.start_sync_loop()?;
                } else if i == 2 {
                    let d = local::LocalDaemon::new(c, cl.clone(), v, rdir_id.clone())?;

                    d.start()?;
                } else {
                    let d = remote::RemoteDaemon::new(c.clone(), cl.clone(), v, rdir_id.clone())?;

                    // TODO: Make certain path for the trayicon (e.g. in /opt)
                    let tray = Tray::setup("./trayicon.png", d, rdir_id, c.local_dir)?;
                    tray.start();
                }

                Ok(())
            })?;

        threads.push(daemon);
    }

    for t in threads {
        // TODO: I hate Result<Result<...>> for t.join()
        let name = t.thread().name().unwrap_or("no_name").to_string();
        let started = t.join();
        if let Err(e) = started {
            bail!("Fatal error in a thread {:?}.\nDetails: {:#?}", name, e);
        }
    }

    Ok(())
}

fn get_remote_dir(name: &String, drive_ref: &mut Arc<Mutex<Client>>) -> Result<File> {
    let mut drive;

    loop {
        if let Ok(d) = drive_ref.try_lock() {
            drive = d;
            break;
        }
    }

    match drive.list_files(
        Some(&format!("name = '{}'", &name)),
        Some("files(id, mimeType)"),
    ) {
        Ok(list) => {
            if list.files.len() == 0 {
                bail!("No file with name '{}' found in your drive", name);
            }

            let root = &list.files[0];

            if root.mime_type.as_ref().unwrap().as_str() != "application/vnd.google-apps.folder" {
                bail!(
                    "Please, make sure that file '{}' on your drive is really a directory",
                    name
                );
            }

            Ok(root.clone())
        }
        Err(e) => {
            if let Some(err) = e.downcast_ref::<DriveError>() {
                match err {
                    DriveError::Unauthorized => {
                        match update_for_shared_client(&mut drive) {
                            Ok(_) => {
                                println!("Info: Client authorization was updated since it was out of date.")
                            }
                            Err(e) => bail!(e),
                        }
                    }
                    _ => {}
                }
            }

            bail!(
                "Fail! Unable to obtain information about the remote root directory.. \nDetails: {}",
                e
            );
        }
    }
}

fn setup_client(conf_dir: &PathBuf) -> Result<Client> {
    let session_file = conf_dir.join("session.toml");
    let creds_file = conf_dir.join("creds.toml");

    let session;
    let creds = files::read_toml::<Creds>(creds_file)?;

    match files::read_toml::<Session>(session_file.clone()) {
        Ok(s) => {
            session = s;
        }
        Err(_) => bail!("Unable to read access authorization data.\nTip: Try to run `ocean-drive auth` to update authorization data"),
    };

    let mut client = Client::new(
        creds.client_id.clone(),
        creds.client_secret.clone(),
        "https://localhost:8080".to_string(),
    );

    client.set_session(session.clone());

    if session.refresh_token.is_some() {
        match client.refresh_token() {
            Ok(new_session) => {
                files::write_toml(new_session, session_file)?;

                println!("Info: Authorization for client is updated.");
            }
            Err(_) => eprintln!("Warn: App was unable to update Google API Access Token.\nTip: Try to manually authorize using `ocean-drive auth`."),
        };
    } else {
        println!("Warn: No refresh token for client is provided!\nPerhaps, it's good to run `ocean-drive auth` to updates your tokens.");
    }

    Ok(client)
}
