//! Embeds a Windows version resource and application manifest.
//!
//! An unsigned executable carrying no version information at all is a strong
//! signal to antivirus machine-learning classifiers. wmux additionally does
//! several things that look like a remote access trojan from the outside — it
//! spawns detached console-less processes, opens named pipes, manipulates
//! console handles, and writes keystrokes into a pseudo-terminal — so it needs
//! every legitimacy signal it can get. Version 0.3.1 was detected as
//! `Trojan:Win32/Wacatac.B!ml`, which blocked both Smart App Control and the
//! winget submission.
//!
//! This does not replace code signing, which is the real fix. It removes the
//! easiest thing to hold against the binary.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Only meaningful on Windows targets; the resource compiler does not exist
    // elsewhere.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource.set("ProductName", "wmux");
    resource.set(
        "FileDescription",
        "Terminal session persistence for Windows",
    );
    resource.set("CompanyName", "HomeOps");
    resource.set(
        "LegalCopyright",
        "Copyright (c) 2026 HomeOps. MIT licensed.",
    );
    resource.set("OriginalFilename", "wmux.exe");
    resource.set("InternalName", "wmux");
    resource.set("Comments", "https://github.com/HomeOps/wmux");

    // Declaring asInvoker states plainly that wmux never wants elevation, and
    // marks it long-path aware so deep session paths behave.
    resource.set_manifest(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
    </windowsSettings>
  </application>
</assembly>
"#,
    );

    // A missing resource compiler must not break the build for contributors
    // without the Windows SDK. CI has it, and the release binary is built
    // there, so a local build simply goes without the metadata.
    if let Err(error) = resource.compile() {
        println!("cargo:warning=could not embed the version resource: {error}");
        println!("cargo:warning=the binary will build, but without version metadata");
    }
}
