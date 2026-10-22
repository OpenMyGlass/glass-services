// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/**
 * The messages of proto/glass/services/v1/services.proto that the glue sends and reads. Field
 * numbers are copied from that file; unknown fields and frames are skipped.
 */
public final class Frames {
    private Frames() {}

    public static final int API_VERSION = 1;

    // Topic
    public static final int TOPIC_GESTURE = 1;
    public static final int TOPIC_SCREEN = 4;
    public static final int TOPIC_BUTTON = 7;

    // ScreenRequest.Action
    public static final int ACTION_WAKE = 1;
    public static final int ACTION_SLEEP = 2;
    public static final int ACTION_USER_ACTIVITY = 5;

    // Screen
    public static final int SCREEN_UNKNOWN = 0;
    public static final int SCREEN_ON = 1;
    public static final int SCREEN_DIM = 2;
    public static final int SCREEN_OFF = 3;

    // GestureKind
    public static final int GESTURE_TAP = 1;
    public static final int GESTURE_LONG_PRESS = 2;
    public static final int GESTURE_SWIPE_FORWARD = 3;
    public static final int GESTURE_SWIPE_BACKWARD = 4;
    public static final int GESTURE_SWIPE_DOWN = 5;
    public static final int GESTURE_SWIPE_UP = 6;

    // ButtonId, ButtonAction
    public static final int BUTTON_CAMERA = 1;
    public static final int BUTTON_POWER = 2;
    public static final int BUTTON_PRESS = 1;
    public static final int BUTTON_LONG_PRESS = 2;
    public static final int BUTTON_RELEASE = 3;

    // ClientFrame fields
    private static final int CF_HELLO = 1;
    private static final int CF_SUBSCRIBE = 2;
    private static final int CF_ACQUIRE_FOCUS = 3;
    private static final int CF_RELEASE_FOCUS = 4;
    private static final int CF_SCREEN = 5;
    private static final int CF_GET_STATE = 7;

    // ServiceFrame fields
    private static final int SF_WELCOME = 1;
    private static final int SF_ERROR = 2;
    private static final int SF_GESTURE = 3;
    private static final int SF_SCREEN_STATE = 9;
    private static final int SF_FOCUS_STATE = 11;
    private static final int SF_BUTTON = 14;

    public static byte[] hello(String clientName) {
        return new Wire.Writer()
                .message(CF_HELLO, new Wire.Writer().uint(1, API_VERSION).string(2, clientName))
                .toBytes();
    }

    public static byte[] subscribe(int... topics) {
        return new Wire.Writer().message(CF_SUBSCRIBE, new Wire.Writer().packed(1, topics)).toBytes();
    }

    public static byte[] acquireFocus() {
        return new Wire.Writer().message(CF_ACQUIRE_FOCUS, new Wire.Writer()).toBytes();
    }

    public static byte[] releaseFocus() {
        return new Wire.Writer().message(CF_RELEASE_FOCUS, new Wire.Writer()).toBytes();
    }

    public static byte[] getState() {
        return new Wire.Writer().message(CF_GET_STATE, new Wire.Writer()).toBytes();
    }

    public static byte[] screenRequest(int action) {
        return new Wire.Writer().message(CF_SCREEN, new Wire.Writer().uint(1, action)).toBytes();
    }

    /** A decoded service frame; {@link #kind} says which fields are meaningful. */
    public static final class Event {
        public enum Kind { WELCOME, ERROR, GESTURE, SCREEN_STATE, FOCUS_STATE, BUTTON, OTHER }

        public Kind kind = Kind.OTHER;
        // Welcome
        public int apiVersion;
        public String serviceVersion = "";
        public final List<String> capabilities = new ArrayList<String>();
        // ServiceError
        public int errorCode;
        public String message = "";
        // Gesture
        public int gesture;
        public int fingers;
        // ScreenState
        public int screen;
        public String reason = "";
        // FocusState
        public boolean focused;
        // Button
        public int button;
        public int buttonAction;
        public boolean screenOff;

        @Override
        public String toString() {
            switch (kind) {
                case WELCOME: return "Welcome(v" + apiVersion + " " + serviceVersion + " " + capabilities + ")";
                case ERROR: return "Error(" + errorCode + " " + message + ")";
                case GESTURE: return "Gesture(" + gesture + " x" + fingers + ")";
                case SCREEN_STATE: return "ScreenState(" + screen + " " + reason + ")";
                case FOCUS_STATE: return "FocusState(" + focused + ")";
                case BUTTON: return "Button(" + button + " " + buttonAction + ")";
                default: return "Other";
            }
        }
    }

    /** Decodes a ServiceFrame. */
    public static Event parse(byte[] frame) throws IOException {
        Event e = new Event();
        Wire.Reader r = new Wire.Reader(frame);
        while (r.next()) {
            if (r.type() != Wire.BYTES) {
                r.skip();
                continue;
            }
            switch (r.field()) {
                case SF_WELCOME: {
                    e.kind = Event.Kind.WELCOME;
                    Wire.Reader m = r.message();
                    while (m.next()) {
                        if (m.field() == 1 && m.type() == Wire.VARINT) e.apiVersion = (int) m.uint();
                        else if (m.field() == 2 && m.type() == Wire.BYTES) e.serviceVersion = m.string();
                        else if (m.field() == 3 && m.type() == Wire.BYTES) e.capabilities.add(m.string());
                        else m.skip();
                    }
                    break;
                }
                case SF_ERROR: {
                    e.kind = Event.Kind.ERROR;
                    Wire.Reader m = r.message();
                    while (m.next()) {
                        if (m.field() == 1 && m.type() == Wire.VARINT) e.errorCode = (int) m.uint();
                        else if (m.field() == 2 && m.type() == Wire.BYTES) e.message = m.string();
                        else m.skip();
                    }
                    break;
                }
                case SF_GESTURE: {
                    e.kind = Event.Kind.GESTURE;
                    Wire.Reader m = r.message();
                    while (m.next()) {
                        if (m.field() == 1 && m.type() == Wire.VARINT) e.gesture = (int) m.uint();
                        else if (m.field() == 2 && m.type() == Wire.VARINT) e.fingers = (int) m.uint();
                        else m.skip();
                    }
                    break;
                }
                case SF_SCREEN_STATE: {
                    e.kind = Event.Kind.SCREEN_STATE;
                    Wire.Reader m = r.message();
                    while (m.next()) {
                        if (m.field() == 1 && m.type() == Wire.VARINT) e.screen = (int) m.uint();
                        else if (m.field() == 2 && m.type() == Wire.BYTES) e.reason = m.string();
                        else m.skip();
                    }
                    break;
                }
                case SF_FOCUS_STATE: {
                    e.kind = Event.Kind.FOCUS_STATE;
                    Wire.Reader m = r.message();
                    while (m.next()) {
                        if (m.field() == 1 && m.type() == Wire.VARINT) e.focused = m.bool();
                        else m.skip();
                    }
                    break;
                }
                case SF_BUTTON: {
                    e.kind = Event.Kind.BUTTON;
                    Wire.Reader m = r.message();
                    while (m.next()) {
                        if (m.field() == 1 && m.type() == Wire.VARINT) e.button = (int) m.uint();
                        else if (m.field() == 2 && m.type() == Wire.VARINT) e.buttonAction = (int) m.uint();
                        else if (m.field() == 4 && m.type() == Wire.VARINT) e.screenOff = m.bool();
                        else m.skip();
                    }
                    break;
                }
                default:
                    r.skip();
            }
        }
        return e;
    }
}
