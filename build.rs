fn main() {
    println!("cargo:rerun-if-changed=rfm.manifest");
    println!("cargo:rerun-if-changed=rfm.rc");
    println!("cargo:rerun-if-changed=assets/rfm.ico");
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=assets/licenses.txt");
    embed_resource::compile_for("rfm.rc", ["rfm"], embed_resource::NONE)
        .manifest_required()
        .expect("compile rfm application and chrome icon resources");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let root = std::env::var("CARGO_MANIFEST_DIR").expect("package directory");
        println!("cargo:rustc-link-arg-bin=rfm=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=rfm=/MANIFESTINPUT:{root}/rfm.manifest");
    }
}
