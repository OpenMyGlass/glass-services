# SPDX-License-Identifier: Apache-2.0
# glass-services for an AOSP tree (Android 4.4 and 6.0): prebuilt modules from the dist directory
# that glass-services' tools/android-dist.sh writes (see android/README.md there). This file is
# part of that directory, which goes to vendor/openmyglass/glass-services.

LOCAL_PATH := $(call my-dir)

# The daemon (docs/api.md), started by init.glassd.rc.
include $(CLEAR_VARS)
LOCAL_MODULE := glassd
LOCAL_MODULE_CLASS := EXECUTABLES
LOCAL_MODULE_TAGS := optional
LOCAL_SRC_FILES := bin/glassd
include $(BUILD_PREBUILT)

# The Android glue (docs/api.md "Android glue"): a persistent system app. AOSP signs it with the
# platform key, which grants DEVICE_POWER, INJECT_EVENTS and SHUTDOWN to the system uid.
include $(CLEAR_VARS)
LOCAL_MODULE := GlassServicesGlue
LOCAL_MODULE_CLASS := APPS
LOCAL_MODULE_TAGS := optional
LOCAL_SRC_FILES := app/GlassServicesGlue.apk
LOCAL_MODULE_SUFFIX := $(COMMON_ANDROID_PACKAGE_SUFFIX)
LOCAL_CERTIFICATE := platform
LOCAL_PRIVILEGED_MODULE := true
include $(BUILD_PREBUILT)
