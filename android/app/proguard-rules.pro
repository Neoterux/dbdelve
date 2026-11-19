# JNA finds native functions and builds structures by reflection, which R8
# cannot follow: keep it whole. Its desktop-only AWT support is absent on
# Android.
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-dontwarn java.awt.**

# The UniFFI bindings: JNA maps their interfaces and callback structures to
# the Rust library by name, so neither may be renamed or removed.
-keep class uniffi.dbdelve_core.** { *; }

# No debug code in a release: logging calls are removed outright.
-assumenosideeffects class android.util.Log {
    public static int v(...);
    public static int d(...);
    public static int i(...);
    public static int w(...);
    public static int e(...);
    public static boolean isLoggable(java.lang.String, int);
}
