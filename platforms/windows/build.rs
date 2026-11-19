fn main() {
    // Nothing to embed anywhere but Windows, where this crate is only a stub.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-changed=res/app.ico");
    embed_resource::compile("res/app.rc", embed_resource::NONE).manifest_required().unwrap();

    // Windows loads every library an exe names before its first line runs, one after another.
    // These are big, and each is only needed later or on another thread: the graphics stack is
    // loaded by the thread that makes the device, while the window opens and the photo decodes;
    // Media Foundation only for videos; the shader compiler only for 3D models; WinHTTP only when
    // looking for a new version. Loading them when
    // they're first called takes that time off every start.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for dll in ["d3d11.dll", "d2d1.dll", "dwrite.dll", "d3dcompiler_47.dll", "mfplat.dll", "propsys.dll", "winhttp.dll"] {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
