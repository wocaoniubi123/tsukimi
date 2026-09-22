//! Runtime setup for the portable Windows build.
//!
//! GTK, GLib, gdk-pixbuf and GStreamer all locate their data files by deriving
//! an installation prefix from the library that is running. That works when
//! the libraries sit in a MSYS2 tree, but this build ships them next to
//! `tsukimi.exe`, so the paths are pointed at the application directory
//! explicitly. Every step is skipped when the directory is missing, which
//! keeps a development build that uses a system MSYS2 untouched.

use std::{
    env,
    fs,
    path::{
        Path,
        PathBuf,
    },
};

/// The directory holding the running executable.
pub fn app_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// Data file the GDK pixbuf loader cache ships with, replaced by the real
/// application path at startup because the cache stores absolute paths.
const APPDIR_TOKEN: &str = "@APPDIR@";

pub fn prepare_environment() {
    let Some(dir) = app_dir() else {
        return;
    };

    // GSettings schemas: GTK aborts without its own, libadwaita without its
    // own, and the client needs its own for the account settings.
    let schemas = dir.join("share/glib-2.0/schemas");
    if schemas.join("gschemas.compiled").exists() {
        set_env("GSETTINGS_SCHEMA_DIR", &schemas);
    }

    // Icon themes live under <prefix>/share/icons, which GTK finds through the
    // XDG data directories.
    let share = dir.join("share");
    if share.join("icons").exists() {
        set_env("XDG_DATA_DIRS", &share);
    }

    // GStreamer plugins.
    let plugins = dir.join("lib/gstreamer-1.0");
    if plugins.exists() {
        set_env("GST_PLUGIN_PATH_1_0", &plugins);
    }

    // gdk-pixbuf loaders. The cache generated at build time refers to the CI
    // prefix, so the paths are rewritten to this installation before use.
    let loaders = dir.join("lib/gdk-pixbuf-2.0/2.10.0/loaders");
    let cache = loaders.join("loaders.cache");
    if cache.exists() {
        match patch_loader_cache(&cache, &dir) {
            Some(patched) => set_env("GDK_PIXBUF_MODULE_FILE", &patched),
            None => set_env("GDK_PIXBUF_MODULE_FILE", &cache),
        }
    }
}

fn patch_loader_cache(cache: &Path, dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(cache).ok()?;
    if !contents.contains(APPDIR_TOKEN) {
        return None;
    }

    let dir = dir.to_string_lossy().replace('\\', "/");
    let patched = contents.replace(APPDIR_TOKEN, &dir);
    let target = env::temp_dir().join("tsukimi-gdk-pixbuf-loaders.cache");
    fs::write(&target, patched).ok()?;
    Some(target)
}

fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: called before any thread that reads the environment is started.
    unsafe { env::set_var(key, value.as_ref()) };
}

/// Directory holding the gettext catalogues, when the portable package ships
/// them next to the executable.
pub fn locale_dir() -> Option<PathBuf> {
    let dir = app_dir()?.join("share/locale");
    dir.is_dir().then_some(dir)
}

/// gettext resolves translations through the POSIX locale variables, which an
/// ordinary Windows session does not set, so seed them from the language GLib
/// derived from the system. Both variables matter: `LANGUAGE` is the priority
/// list gettext prefers, and `LANG` keeps the locale from looking like "C",
/// which would make gettext ignore `LANGUAGE` altogether.
pub fn seed_language() {
    if env::var_os("LANGUAGE").is_some()
        || env::var_os("LC_ALL").is_some()
        || env::var_os("LANG").is_some()
    {
        return;
    }

    let names: Vec<String> = gtk::glib::language_names()
        .iter()
        .map(|name| name.to_string())
        .filter(|name| !name.is_empty() && name != "C")
        .collect();

    let Some((first, _)) = names.split_first() else {
        return;
    };

    set_env("LANG", first);
    set_env("LANGUAGE", names.join(":"));
}
