use std::{env, fs, path::PathBuf, process::Command};

fn resource_path(path: PathBuf) -> String {
    let path = path.canonicalize().expect("resolve resource path");
    let path = path.to_string_lossy();
    path.strip_prefix(r"\\?\")
        .unwrap_or(&path)
        .replace('\\', "\\\\")
}

fn msvc_rc() -> PathBuf {
    if let Some(path) = env::var_os("RC") {
        return path.into();
    }
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return "rc.exe".into();
    }
    let sdk = env::var_os("WindowsSdkDir")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("ProgramFiles(x86)").map(|p| PathBuf::from(p).join("Windows Kits/10"))
        })
        .expect("Windows SDK absent; run from a VS developer shell or set RC");
    let mut versions: Vec<_> = fs::read_dir(sdk.join("bin"))
        .expect("Windows SDK bin directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("x64/rc.exe").is_file())
        .collect();
    versions.sort();
    versions
        .pop()
        .expect("Windows SDK x64 rc.exe absent; set RC")
        .join("x64/rc.exe")
}

fn main() {
    println!("cargo:rerun-if-changed=../../assets/app-icon.ico");
    println!("cargo:rerun-if-changed=../../assets/github-mark.ico");
    println!("cargo:rerun-if-env-changed=RC");
    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }
    let icon =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../assets/app-icon.ico");
    if !icon.is_file() {
        println!(
            "cargo:warning=app-icon.ico absent; GUI temporarily uses Windows application icon"
        );
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = resource_path(icon);
    let manifest = out.join("gui.manifest");
    fs::write(&manifest, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="WeChatWinH.ControlPanel" type="win32"/>
<description>微win</description>
<dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
<trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="highestAvailable" uiAccess="false"/></requestedPrivileges></security></trustInfo>
<application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness><longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware></windowsSettings></application>
</assembly>"#).expect("write GUI manifest");
    let manifest_path = resource_path(manifest);
    let github = resource_path(
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("../../assets/github-mark.ico"),
    );
    let rc = out.join("gui.rc");
    fs::write(
        &rc,
        format!("#pragma code_page(65001)\n100 ICON \"{icon}\"\n101 ICON \"{github}\"\n1 24 \"{manifest_path}\"\n"),
    )
    .expect("write GUI resources");
    if target.ends_with("msvc") {
        let object = out.join("gui-resources.res");
        let status = Command::new(msvc_rc())
            .args(["/nologo", "/c65001"])
            .arg(format!("/fo{}", object.display()))
            .arg(&rc)
            .status()
            .expect("run Windows SDK rc.exe (set RC to its path)");
        assert!(status.success(), "GUI MSVC resource compilation failed");
        println!("cargo:rustc-link-arg-bin=微win={}", object.display());
        // Resource #1 contains the requested privileges and PerMonitorV2 manifest.
        println!("cargo:rustc-link-arg-bin=微win=/MANIFEST:NO");
        return;
    }
    let object = out.join("gui-resources.o");
    let compiler = env::var_os("RC").unwrap_or_else(|| {
        if env::var("HOST").unwrap_or_default().contains("windows") {
            "windres".into()
        } else {
            "x86_64-w64-mingw32-windres".into()
        }
    });
    let status = Command::new(compiler)
        .args(["-i"])
        .arg(rc)
        .args(["-O", "coff", "-o"])
        .arg(&object)
        .status()
        .expect("run windres (set RC to its path)");
    assert!(status.success(), "GUI resource compilation failed");
    println!("cargo:rustc-link-arg-bin=微win={}", object.display());
}
