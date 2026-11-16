# JNA and the UniFFI bindings are reached reflectively from native code;
# R8 must not rename or strip them in consuming apps.
-dontwarn java.awt.*
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-keep class app.nolva.core.** { *; }
