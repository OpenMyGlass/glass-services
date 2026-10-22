# SPDX-License-Identifier: Apache-2.0
# Product makefile of glass-services: inherit it from the device makefile with
#   $(call inherit-product, vendor/openmyglass/glass-services/glass-services.mk)
# and import /init.glassd.rc from the device's init.<hardware>.rc (android/README.md).

GLASS_SERVICES_DIR ?= vendor/openmyglass/glass-services

ifneq ($(words $(wildcard $(GLASS_SERVICES_DIR)/bin/glassd $(GLASS_SERVICES_DIR)/app/GlassServicesGlue.apk)),2)
$(error $(GLASS_SERVICES_DIR) has no glassd or GlassServicesGlue.apk: copy the dist directory of glass-services' tools/android-dist.sh (or its CI artifact glass-services-android-dist) there)
endif

PRODUCT_PACKAGES += \
    glassd \
    GlassServicesGlue

# init.glassd.rc is imported from the ramdisk: Android 4.4 and 6.0 read no rc files from /system.
PRODUCT_COPY_FILES += \
    $(GLASS_SERVICES_DIR)/init.glassd.rc:root/init.glassd.rc \
    $(GLASS_SERVICES_DIR)/keylayout/twl6030_pwrbutton.kl:system/usr/keylayout/twl6030_pwrbutton.kl \
    $(GLASS_SERVICES_DIR)/idc/sensor00fn11.idc:system/usr/idc/sensor00fn11.idc
