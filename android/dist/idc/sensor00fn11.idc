# SPDX-License-Identifier: Apache-2.0
# The Glass touchpad (docs/specs/touchpad-gestures.md). glassd reads it and turns gestures into
# key events for regular apps (docs/api.md "Android glue") and grabs it, so Android gets no events
# from it. Should the grab be turned off (--grab-touchpad off), this keeps it a touch navigation
# device: no pointer on screen, no screen coordinates.
touch.deviceType = touchNavigation
device.internal = 1
