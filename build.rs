use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

const DEFAULT_HUB_NAME: &str = "HarvestHub-Dev";
const IDENTITY_FILE_NAME: &str = "harvesthub-dev-identity.env";

#[derive(Debug)]
struct HubIdentity {
    device_id: String,
    hub_secret: String,
}

fn main() {
    embuild::espidf::sysenv::output();

    let hub_name = env::var("HUB_NAME").unwrap_or_else(|_| DEFAULT_HUB_NAME.to_string());
    let identity = env_identity().unwrap_or_else(generated_identity);

    println!("cargo:rustc-env=HUB_DEVICE_ID={}", identity.device_id);
    println!("cargo:rustc-env=HUB_SECRET={}", identity.hub_secret);
    println!("cargo:rustc-env=HUB_NAME={hub_name}");

    print_setup_payload(&identity, &hub_name);
}

fn env_identity() -> Option<HubIdentity> {
    let device_id = env::var("HUB_DEVICE_ID").ok()?;
    let hub_secret = env::var("HUB_SECRET").ok()?;
    Some(HubIdentity {
        device_id,
        hub_secret,
    })
}

fn generated_identity() -> HubIdentity {
    let identity_path = identity_file_path();

    if let Ok(content) = fs::read_to_string(&identity_path) {
        if let Some(identity) = parse_identity_file(&content) {
            return identity;
        }
    }

    let identity = HubIdentity {
        device_id: generate_uuid_v4(),
        hub_secret: generate_hex_secret(),
    };

    if let Some(parent) = identity_path.parent() {
        fs::create_dir_all(parent).expect("create target directory for hub identity");
    }

    fs::write(
        &identity_path,
        format!(
            "HUB_DEVICE_ID={}\nHUB_SECRET={}\n",
            identity.device_id, identity.hub_secret
        ),
    )
    .expect("write generated hub identity");

    identity
}

fn identity_file_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest_dir.join("target").join(IDENTITY_FILE_NAME)
}

fn parse_identity_file(content: &str) -> Option<HubIdentity> {
    let mut device_id = None;
    let mut hub_secret = None;

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("HUB_DEVICE_ID=") {
            device_id = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("HUB_SECRET=") {
            hub_secret = Some(value.to_string());
        }
    }

    Some(HubIdentity {
        device_id: device_id?,
        hub_secret: hub_secret?,
    })
}

fn print_setup_payload(identity: &HubIdentity, hub_name: &str) {
    let setup_uri = format!(
        "harvesthub://hub-setup?hub_uuid={}&hub_secret={}&hub_name={}",
        identity.device_id,
        identity.hub_secret,
        encode_uri_component(hub_name),
    );

    println!(
        "cargo:warning=HarvestHub setup hub_uuid={}",
        identity.device_id
    );
    println!(
        "cargo:warning=HarvestHub setup hub_secret={}",
        identity.hub_secret
    );
    println!("cargo:warning=HarvestHub setup uri={setup_uri}");
}

fn generate_uuid_v4() -> String {
    let mut bytes = [0_u8; 16];
    fill_random(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        write!(&mut out, "{byte:02x}").expect("write UUID hex");
    }
    out
}

fn generate_hex_secret() -> String {
    let mut bytes = [0_u8; 32];
    fill_random(&mut bytes);
    hex_string(&bytes)
}

fn fill_random(bytes: &mut [u8]) {
    let mut file = fs::File::open(Path::new("/dev/urandom")).expect("open /dev/urandom");
    file.read_exact(bytes).expect("read random bytes");
}

fn hex_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex");
    }
    out
}

fn encode_uri_component(value: &str) -> String {
    let mut out = String::new();

    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            write!(&mut out, "%{byte:02X}").expect("write URI escape");
        }
    }

    out
}
