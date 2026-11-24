# Whisker's copy of i-slint-backend-android-activity 1.19.0

This is the crate as published on crates.io, with two changes. The first, in
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

The second, in `java/SlintAndroidJavaHelper.java` (`SlintInputView.setText` and
`onCreateInputConnection`): when the app rewrites the text (clears it after
sending, or replaces a word: autocorrect, a :shortcode:), the keyboard is restarted
(`restartInput`) with a fresh editable, and on Android 11+ is given the text
around the cursor when it connects. Upstream only told the keyboard the cursor
had moved, so it kept composing from the old text: a sent message came back at
the start of the next one, autocorrect worked from stale words, and capitals
came up in the middle of words.

Text that Slint says back while the keyboard is already further on (an
older text the keyboard itself sent in the last 1.5 s, as when delete is
held down) is left alone rather than taken as the app's change: restarting
the keyboard for it stopped a held key from repeating.

Deleting and typing by key (a keyboard holding delete sends key presses,
which Slint carries out and says back) is passed on as upstream does, not
restarted: a restart for each one stopped a held key after one character.
