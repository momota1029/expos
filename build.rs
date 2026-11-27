fn main() {
    println!("cargo:rerun-if-changed=assets/icon/expos.rc");
    println!("cargo:rerun-if-changed=assets/icon/expos.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/icon/expos.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
