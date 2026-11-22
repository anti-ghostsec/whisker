# Whisker's copy of i-slint-backend-android-activity 1.19.0

This is the crate as published on crates.io, with one change, in
`javahelper.rs` (`build_dex_class_loader`): the Java helper
(`dev.slint.android.SlintAndroidJavaHelper`) is first looked for with the app's
own class loader, and only loaded the upstream way if it isn't there.

Upstream always loads the helper from bytes inside the native library
(`InMemoryDexClassLoader`, or a file written to the code cache before
Android 8). Hardened systems such as GrapheneOS can refuse apps both kinds of
dynamic code loading, and Whisker then didn't start. Whisker's build puts the
same helper into the APK as `classes.dex` (see `.github/workflows`), where the
app's class loader finds it and nothing is loaded dynamically.

Drop this copy once Slint can use a helper shipped in the APK itself.
