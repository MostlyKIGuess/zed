use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
};

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    let (lib_name, download_name) = match (target_os.as_str(), target_arch.as_str()) {
        ("linux", "x86_64") => ("libpdfium.so", "pdfium-linux-x64"),
        ("linux", "aarch64") => ("libpdfium.so", "pdfium-linux-arm64"),
        ("macos", "x86_64") => ("libpdfium.dylib", "pdfium-mac-x64"),
        ("macos", "aarch64") => ("libpdfium.dylib", "pdfium-mac-arm64"),
        ("windows", "x86_64") => ("pdfium.dll", "pdfium-win-x64"),
        _ => {
            println!(
                "cargo:warning=PDF image rendering: Unsupported platform {} {}",
                target_os, target_arch
            );
            return;
        }
    };

    // Download to the crate's lib directory (accessible at runtime)
    // This is taken from https://github.com/lailogue/rust-pdf-viewer
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let lib_dir = Path::new(&crate_dir).join("lib");
    let lib_path = lib_dir.join(lib_name);

    if lib_path.exists() {
        println!("cargo:rustc-link-search={}", lib_dir.display());
        println!("cargo:rerun-if-changed={}", lib_path.display());
        println!(
            "cargo:warning=Using existing pdfium library at {}",
            lib_path.display()
        );
        return;
    }

    fs::create_dir_all(&lib_dir).unwrap();

    let url = format!(
        "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/{}.tgz",
        download_name
    );

    println!("cargo:warning=Downloading pdfium from {}", url);

    match download_and_extract_pdfium(&url, &lib_dir, lib_name) {
        Ok(()) => {
            println!("cargo:rustc-link-search={}", lib_dir.display());
            // Tell cargo to rerun build script if the library changes
            println!("cargo:rerun-if-changed={}", lib_path.display());
            println!(
                "cargo:warning=Successfully downloaded pdfium to {}",
                lib_path.display()
            );
        }
        Err(e) => {
            println!("cargo:warning=Failed to download pdfium: {}", e);
            println!("cargo:warning=PDF image rendering may not work properly.");
        }
    }
}

fn download_and_extract_pdfium(url: &str, lib_dir: &Path, lib_name: &str) -> io::Result<()> {
    let response = reqwest::blocking::get(url)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Download failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("HTTP error: {}", response.status()),
        ));
    }

    let mut reader = flate2::read::GzDecoder::new(response);
    let mut archive = tar::Archive::new(&mut reader);

    // extract
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;

        // library file in the archive
        if path.ends_with(lib_name) {
            let lib_path = lib_dir.join(lib_name);
            let mut file = File::create(&lib_path)?;
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer)?;
            file.write_all(&buffer)?;

            // executable permissions
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&lib_path)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&lib_path, perms)?;
            }

            return Ok(());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found in archive", lib_name),
    ))
}
