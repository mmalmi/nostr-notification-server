use std::fs;
use std::path::Path;
use std::process::Command;
use nostr_sdk::base64::{Engine as _, engine::general_purpose};
use log::debug;
use std::error::Error;

const PRIVATE_KEY_PATH: &str = "config/vapid_private_key.pem";
const PUBLIC_KEY_PATH: &str = "config/vapid_public_key.txt";

pub fn ensure_vapid_keys() -> Result<(String, String), Box<dyn Error>> {
    if !Path::new(PRIVATE_KEY_PATH).exists() {
        debug!("Generating new VAPID keys");
        generate_vapid_keys()?;
    }

    let private_key = fs::read_to_string(PRIVATE_KEY_PATH)?;
    let public_key = fs::read_to_string(PUBLIC_KEY_PATH)?;
    
    Ok((private_key, public_key.trim().to_string()))
}

fn generate_vapid_keys() -> Result<(), Box<dyn std::error::Error>> {
    debug!("Generating private key with OpenSSL");
    
    // Generate private key directly in PEM format
    Command::new("openssl")
        .args(&["ecparam", "-genkey", "-name", "prime256v1", "-out", PRIVATE_KEY_PATH])
        .output()?;

    // Extract the raw public key bytes
    let public_key_output = Command::new("openssl")
        .args(&["ec", 
               "-in", PRIVATE_KEY_PATH,
               "-pubout", 
               "-outform", "DER",
               "-conv_form", "uncompressed"])
        .output()?;

    if !public_key_output.status.success() {
        let error = String::from_utf8_lossy(&public_key_output.stderr);
        return Err(format!("Failed to generate public key: {}", error).into());
    }

    // Extract the 65-byte public key from the DER structure
    // DER encoded public key starts with a sequence and bit string headers
    // The actual key starts at offset 26 and is 65 bytes long
    let raw_public_key = &public_key_output.stdout[26..91];

    // URL-safe base64 encode the raw public key bytes
    let public_key = general_purpose::URL_SAFE_NO_PAD.encode(raw_public_key);
    fs::write(PUBLIC_KEY_PATH, &public_key)?;

    Ok(())
}
