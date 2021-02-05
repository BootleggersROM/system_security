// Copyright 2020, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module holds global state of Keystore such as the thread local
//! database connections and connections to services that Keystore needs
//! to talk to.

use crate::gc::Gc;
use crate::legacy_blob::LegacyBlobLoader;
use crate::super_key::SuperKeyManager;
use crate::utils::Asp;
use crate::{async_task::AsyncTask, database::MonotonicRawTime};
use crate::{
    database::KeystoreDB,
    database::Uuid,
    error::{map_binder_status, map_binder_status_code, Error, ErrorCode},
};
use crate::{enforcements::Enforcements, error::map_km_error};
use android_hardware_security_keymint::aidl::android::hardware::security::keymint::{
    KeyMintHardwareInfo::KeyMintHardwareInfo, SecurityLevel::SecurityLevel,
};
use android_hardware_security_keymint::binder::StatusCode;
use android_security_compat::aidl::android::security::compat::IKeystoreCompatService::IKeystoreCompatService;
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use std::sync::Mutex;
use std::{cell::RefCell, sync::Once};
use std::{collections::HashMap, path::Path, path::PathBuf};

static DB_INIT: Once = Once::new();

/// Open a connection to the Keystore 2.0 database. This is called during the initialization of
/// the thread local DB field. It should never be called directly. The first time this is called
/// we also call KeystoreDB::cleanup_leftovers to restore the key lifecycle invariant. See the
/// documentation of cleanup_leftovers for more details.
fn create_thread_local_db() -> KeystoreDB {
    let mut db = KeystoreDB::new(&DB_PATH.lock().expect("Could not get the database directory."))
        .expect("Failed to open database.");
    DB_INIT.call_once(|| {
        log::info!("Touching Keystore 2.0 database for this first time since boot.");
        db.insert_last_off_body(MonotonicRawTime::now())
            .expect("Could not initialize database with last off body.");
        log::info!("Calling cleanup leftovers.");
        let n = db.cleanup_leftovers().expect("Failed to cleanup database on startup.");
        if n != 0 {
            log::info!(
                concat!(
                    "Cleaned up {} failed entries. ",
                    "This indicates keystore crashed during key generation."
                ),
                n
            );
        }
        Gc::notify_gc();
    });
    db
}

thread_local! {
    /// Database connections are not thread safe, but connecting to the
    /// same database multiple times is safe as long as each connection is
    /// used by only one thread. So we store one database connection per
    /// thread in this thread local key.
    pub static DB: RefCell<KeystoreDB> =
            RefCell::new(create_thread_local_db());
}

#[derive(Default)]
struct DevicesMap {
    devices_by_uuid: HashMap<Uuid, (Asp, KeyMintHardwareInfo)>,
    uuid_by_sec_level: HashMap<SecurityLevel, Uuid>,
}

impl DevicesMap {
    fn dev_by_sec_level(
        &self,
        sec_level: &SecurityLevel,
    ) -> Option<(Asp, KeyMintHardwareInfo, Uuid)> {
        self.uuid_by_sec_level.get(sec_level).and_then(|uuid| self.dev_by_uuid(uuid))
    }

    fn dev_by_uuid(&self, uuid: &Uuid) -> Option<(Asp, KeyMintHardwareInfo, Uuid)> {
        self.devices_by_uuid
            .get(uuid)
            .map(|(dev, hw_info)| ((*dev).clone(), (*hw_info).clone(), *uuid))
    }

    /// The requested security level and the security level of the actual implementation may
    /// differ. So we map the requested security level to the uuid of the implementation
    /// so that there cannot be any confusion as to which KeyMint instance is requested.
    fn insert(&mut self, sec_level: SecurityLevel, dev: Asp, hw_info: KeyMintHardwareInfo) {
        // For now we use the reported security level of the KM instance as UUID.
        // TODO update this section once UUID was added to the KM hardware info.
        let uuid: Uuid = sec_level.into();
        self.devices_by_uuid.insert(uuid, (dev, hw_info));
        self.uuid_by_sec_level.insert(sec_level, uuid);
    }
}

lazy_static! {
    /// The path where keystore stores all its keys.
    pub static ref DB_PATH: Mutex<PathBuf> = Mutex::new(
        Path::new("/data/misc/keystore").to_path_buf());
    /// Runtime database of unwrapped super keys.
    pub static ref SUPER_KEY: SuperKeyManager = Default::default();
    /// Map of KeyMint devices.
    static ref KEY_MINT_DEVICES: Mutex<DevicesMap> = Default::default();
    /// Timestamp service.
    static ref TIME_STAMP_DEVICE: Mutex<Option<Asp>> = Default::default();
    /// A single on-demand worker thread that handles deferred tasks with two different
    /// priorities.
    pub static ref ASYNC_TASK: AsyncTask = Default::default();
    /// Singleton for enforcements.
    pub static ref ENFORCEMENTS: Enforcements = Enforcements::new();
    /// LegacyBlobLoader is initialized and exists globally.
    /// The same directory used by the database is used by the LegacyBlobLoader as well.
    pub static ref LEGACY_BLOB_LOADER: LegacyBlobLoader = LegacyBlobLoader::new(
        &DB_PATH.lock().expect("Could not get the database path for legacy blob loader."));
}

static KEYMINT_SERVICE_NAME: &str = "android.hardware.security.keymint.IKeyMintDevice";

/// Make a new connection to a KeyMint device of the given security level.
/// If no native KeyMint device can be found this function also brings
/// up the compatibility service and attempts to connect to the legacy wrapper.
fn connect_keymint(security_level: &SecurityLevel) -> Result<(Asp, KeyMintHardwareInfo)> {
    let service_name = match *security_level {
        SecurityLevel::TRUSTED_ENVIRONMENT => format!("{}/default", KEYMINT_SERVICE_NAME),
        SecurityLevel::STRONGBOX => format!("{}/strongbox", KEYMINT_SERVICE_NAME),
        _ => {
            return Err(Error::Km(ErrorCode::HARDWARE_TYPE_UNAVAILABLE))
                .context("In connect_keymint.")
        }
    };

    let keymint = map_binder_status_code(binder::get_interface(&service_name))
        .context("In connect_keymint: Trying to connect to genuine KeyMint service.")
        .or_else(|e| {
            match e.root_cause().downcast_ref::<Error>() {
                Some(Error::BinderTransaction(StatusCode::NAME_NOT_FOUND)) => {
                    // This is a no-op if it was called before.
                    keystore2_km_compat::add_keymint_device_service();

                    let keystore_compat_service: Box<dyn IKeystoreCompatService> =
                        map_binder_status_code(binder::get_interface("android.security.compat"))
                            .context("In connect_keymint: Trying to connect to compat service.")?;
                    map_binder_status(keystore_compat_service.getKeyMintDevice(*security_level))
                        .map_err(|e| match e {
                            Error::BinderTransaction(StatusCode::NAME_NOT_FOUND) => {
                                Error::Km(ErrorCode::HARDWARE_TYPE_UNAVAILABLE)
                            }
                            e => e,
                        })
                        .context("In connect_keymint: Trying to get Legacy wrapper.")
                }
                _ => Err(e),
            }
        })?;

    let hw_info = map_km_error(keymint.getHardwareInfo())
        .context("In connect_keymint: Failed to get hardware info.")?;

    Ok((Asp::new(keymint.as_binder()), hw_info))
}

/// Get a keymint device for the given security level either from our cache or
/// by making a new connection. Returns the device, the hardware info and the uuid.
/// TODO the latter can be removed when the uuid is part of the hardware info.
pub fn get_keymint_device(
    security_level: &SecurityLevel,
) -> Result<(Asp, KeyMintHardwareInfo, Uuid)> {
    let mut devices_map = KEY_MINT_DEVICES.lock().unwrap();
    if let Some((dev, hw_info, uuid)) = devices_map.dev_by_sec_level(&security_level) {
        Ok((dev, hw_info, uuid))
    } else {
        let (dev, hw_info) = connect_keymint(security_level).context("In get_keymint_device.")?;
        devices_map.insert(*security_level, dev, hw_info);
        // Unwrap must succeed because we just inserted it.
        Ok(devices_map.dev_by_sec_level(security_level).unwrap())
    }
}

/// Get a keymint device for the given uuid. This will only access the cache, but will not
/// attempt to establish a new connection. It is assumed that the cache is already populated
/// when this is called. This is a fair assumption, because service.rs iterates through all
/// security levels when it gets instantiated.
pub fn get_keymint_dev_by_uuid(uuid: &Uuid) -> Result<(Asp, KeyMintHardwareInfo)> {
    let devices_map = KEY_MINT_DEVICES.lock().unwrap();
    if let Some((dev, hw_info, _)) = devices_map.dev_by_uuid(uuid) {
        Ok((dev, hw_info))
    } else {
        Err(Error::sys()).context("In get_keymint_dev_by_uuid: No KeyMint instance found.")
    }
}

static TIME_STAMP_SERVICE_NAME: &str = "android.hardware.security.secureclock.ISecureClock";

/// Make a new connection to a secure clock service.
/// If no native SecureClock device can be found brings up the compatibility service and attempts
/// to connect to the legacy wrapper.
fn connect_secureclock() -> Result<Asp> {
    let secureclock = map_binder_status_code(binder::get_interface(TIME_STAMP_SERVICE_NAME))
        .context("In connect_secureclock: Trying to connect to genuine secure clock service.")
        .or_else(|e| {
            match e.root_cause().downcast_ref::<Error>() {
                Some(Error::BinderTransaction(StatusCode::NAME_NOT_FOUND)) => {
                    // This is a no-op if it was called before.
                    keystore2_km_compat::add_keymint_device_service();

                    let keystore_compat_service: Box<dyn IKeystoreCompatService> =
                        map_binder_status_code(binder::get_interface("android.security.compat"))
                            .context(
                                "In connect_secureclock: Trying to connect to compat service.",
                            )?;

                    // Legacy secure clock services were only implemented by TEE.
                    map_binder_status(keystore_compat_service.getSecureClock())
                        .map_err(|e| match e {
                            Error::BinderTransaction(StatusCode::NAME_NOT_FOUND) => {
                                Error::Km(ErrorCode::HARDWARE_TYPE_UNAVAILABLE)
                            }
                            e => e,
                        })
                        .context("In connect_secureclock: Trying to get Legacy wrapper.")
                }
                _ => Err(e),
            }
        })?;

    Ok(Asp::new(secureclock.as_binder()))
}

/// Get the timestamp service that verifies auth token timeliness towards security levels with
/// different clocks.
pub fn get_timestamp_service() -> Result<Asp> {
    let mut ts_device = TIME_STAMP_DEVICE.lock().unwrap();
    if let Some(dev) = &*ts_device {
        Ok(dev.clone())
    } else {
        let dev = connect_secureclock().context("In get_timestamp_service.")?;
        *ts_device = Some(dev.clone());
        Ok(dev)
    }
}