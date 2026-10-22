// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

/**
 * Touchpad gestures as key events for regular Android activities (docs/api.md, "Android
 * glue"). Our mapping. Key codes are android.view.KeyEvent's, repeated here so this class
 * stays free of Android imports and runs in JVM tests.
 */
public final class KeyMap {
    public static final int KEYCODE_DPAD_UP = 19;
    public static final int KEYCODE_DPAD_LEFT = 21;
    public static final int KEYCODE_DPAD_RIGHT = 22;
    public static final int KEYCODE_DPAD_CENTER = 23;
    public static final int KEYCODE_BACK = 4;
    public static final int KEYCODE_MENU = 82;

    private KeyMap() {}

    /** The key code for a gesture, or 0 for none (multi-finger gestures are not injected). */
    public static int keyFor(int gesture, int fingers) {
        if (fingers > 1) {
            return 0;
        }
        switch (gesture) {
            case Frames.GESTURE_TAP: return KEYCODE_DPAD_CENTER;
            case Frames.GESTURE_SWIPE_FORWARD: return KEYCODE_DPAD_RIGHT;
            case Frames.GESTURE_SWIPE_BACKWARD: return KEYCODE_DPAD_LEFT;
            case Frames.GESTURE_SWIPE_DOWN: return KEYCODE_BACK;
            case Frames.GESTURE_SWIPE_UP: return KEYCODE_DPAD_UP;
            case Frames.GESTURE_LONG_PRESS: return KEYCODE_MENU;
            default: return 0;
        }
    }
}
