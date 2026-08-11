fn main() {
    println!("cargo:rerun-if-env-changed=TRACEDISK_LICENSE_PUBLIC_KEY");
    if std::env::var("PROFILE").as_deref() == Ok("release")
        && std::env::var("TRACEDISK_LICENSE_PUBLIC_KEY")
            .map(|value| value.trim().is_empty())
            .unwrap_or(true)
    {
        panic!("release 构建必须设置 TRACEDISK_LICENSE_PUBLIC_KEY");
    }
    tauri_build::build()
}
