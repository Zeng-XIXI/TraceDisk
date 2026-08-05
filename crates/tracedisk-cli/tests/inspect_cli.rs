use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn inspects_exfat_image_as_json() {
    let mut image = vec![0_u8; 512 * 32];
    image[3..11].copy_from_slice(b"EXFAT   ");
    image[80..84].copy_from_slice(&24_u32.to_le_bytes());
    image[84..88].copy_from_slice(&8_u32.to_le_bytes());
    image[88..92].copy_from_slice(&32_u32.to_le_bytes());
    image[92..96].copy_from_slice(&100_u32.to_le_bytes());
    image[96..100].copy_from_slice(&2_u32.to_le_bytes());
    image[100..104].copy_from_slice(&0xd1a0_0001_u32.to_le_bytes());
    image[108] = 9;
    image[109] = 3;
    image[110] = 1;
    image[112] = 10;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "tracedisk-cli-test-{}-{unique}.img",
        std::process::id()
    ));
    fs::write(&path, image).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_tracedisk"))
        .args(["inspect", path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    let _ = fs::remove_file(&path);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"partition_scheme\": \"super-floppy\""));
    assert!(stdout.contains("\"filesystem\": \"exFAT\""));
    assert!(stdout.contains("\"volume_serial\": 3516923905"));
}
