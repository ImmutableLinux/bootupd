//! Module with checks related to Secure Boot.
//!
//! This is primarily used for checking whether it is safe to update a system which has Secure
//! Boot enabled. There can be complications if updating to a new version of shim which is only
//! signed by Microsoft keys which the user's system does not have matching certificates for,
//! so in that scenario we just block updates until the firmware is updated to match (most likely by
//! fwupd).
//!
//! See https://github.com/coreos/bootupd/issues/1099

use anyhow::{bail, Result};
use fn_error_context::context;
use log::info;
use virtfw_libefi::efivar::{ids, sigdb::EfiSigDB};
use virtfw_libefi::sb::certs;
use virtfw_libefi::varstore::sysfs;

/// Check whether Secure Boot is enabled.
fn is_secureboot_enabled() -> bool {
    let Some(var) = sysfs::varstore_read(ids::SECURE_BOOT.name, ids::SECURE_BOOT.guid) else {
        info!("Could not read the Secure Boot EFI variable. Assuming Secure Boot is not enabled.");
        return false;
    };
    // First byte represents a boolean for "is_enabled"
    match var.data().first() {
        Some(&b) => b != 0,
        None => false,
    }
}

/// Check whether the firmware's signature database contains the Microsoft UEFI CA 2023 certificate.
fn db_contains_ms_2023_cert() -> Result<bool> {
    let Some(var) = sysfs::varstore_read(ids::DB.name, ids::DB.guid) else {
        // At this stage, we'll have confirmed the user does have Secure Boot enabled.
        // Best to be safe and abort any updates if we can't even read the EFI variable.
        bail!("Could not read the signature database variable. Assuming it is not safe to update.");
    };
    let Some(sigdb) = EfiSigDB::new_from_bytes(var.data()) else {
        bail!("Failed to parse Secure Boot signature database (unknown layout). Assuming it is not safe to update.");
    };

    Ok(sigdb
        .get_x509_list()
        .contains(&certs::MICROSOFT_DB_UEFI_2023))
}

/// Attempt to validate that the system can safely accept an EFI bootloader update (if EFI-booted).
///
/// If Secure Boot is enabled, we can't allow updates if the signature database doesn't contain
/// the Microsoft UEFI CA 2023 certificate.
#[context("Validating Secure Boot certificate compatibility")]
pub(crate) fn validate_secureboot_for_update() -> Result<()> {
    if !crate::efi::is_efi_booted()? {
        info!("Not EFI-booted, skipping Secure Boot certificate check");
        return Ok(());
    }

    if !is_secureboot_enabled() {
        info!("Secure Boot not enabled, skipping Secure Boot certificate check");
        return Ok(());
    };

    match db_contains_ms_2023_cert() {
        Ok(true) => {
            info!("Secure Boot DB contains Microsoft UEFI CA 2023 certificate. Safe to update.");
            Ok(())
        }
        Ok(false) => bail!(
            "Secure Boot is enabled but the Microsoft UEFI CA 2023 certificate was not \
             found in the firmware's signature database. Updating the shim could render this system \
             unbootable. Please update your system firmware by using, for example, fwupd."
        ),
        Err(e) => Err(e),
    }
}
