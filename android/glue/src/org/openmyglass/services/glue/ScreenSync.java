// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

import java.util.ArrayList;
import java.util.List;

/**
 * Keeps Android's screen in step with glassd's screen policy (docs/api.md, "Android glue").
 * Pure: inputs are glassd's ScreenState, Android's screen broadcasts and the connection;
 * outputs are {@link Action}s for the platform. glassd decides; Android only executes, except
 * when the framework turned the screen on or off by itself, which is reported back to glassd.
 */
public final class ScreenSync {
    /** What to do, in order. */
    public enum Action {
        /** Hold the bright screen wake lock (and release the dim one). */
        LOCK_BRIGHT,
        /** Hold the dim screen wake lock (and release the bright one). */
        LOCK_DIM,
        /** Release both wake locks. */
        LOCK_NONE,
        /** PowerManager.wakeUp. */
        WAKE_UP,
        /** PowerManager.goToSleep. */
        GO_TO_SLEEP,
        /** Send ScreenRequest{ACTION_WAKE} to glassd. */
        REQUEST_WAKE,
        /** Send ScreenRequest{ACTION_SLEEP} to glassd. */
        REQUEST_SLEEP,
    }

    /** How long a screen broadcast caused by our own call is expected. */
    public static final long ECHO_MS = 3000;

    private int glassd = Frames.SCREEN_UNKNOWN;
    private boolean interactive;
    private long expectOnUntil = -1;
    private long expectOffUntil = -1;

    /** @param interactive whether Android's screen is on now (PowerManager.isScreenOn). */
    public ScreenSync(boolean interactive) {
        this.interactive = interactive;
    }

    public int glassdScreen() {
        return glassd;
    }

    public boolean interactive() {
        return interactive;
    }

    /** glassd reported its screen state (a change or a snapshot). */
    public List<Action> onGlassdScreen(int screen, long nowMs) {
        List<Action> out = new ArrayList<Action>();
        glassd = screen;
        switch (screen) {
            case Frames.SCREEN_ON:
            case Frames.SCREEN_DIM:
                if (!interactive) {
                    out.add(Action.WAKE_UP);
                    // The expectations are independent: after a quick off and on, the
                    // framework still sends both broadcasts, the off one first.
                    expectOnUntil = nowMs + ECHO_MS;
                    interactive = true;
                }
                out.add(screen == Frames.SCREEN_ON ? Action.LOCK_BRIGHT : Action.LOCK_DIM);
                break;
            case Frames.SCREEN_OFF:
                out.add(Action.LOCK_NONE);
                if (interactive) {
                    out.add(Action.GO_TO_SLEEP);
                    expectOffUntil = nowMs + ECHO_MS;
                    interactive = false;
                }
                break;
            default:
                break;
        }
        return out;
    }

    /** Android's ACTION_SCREEN_ON (true) or ACTION_SCREEN_OFF (false). */
    public List<Action> onAndroidScreen(boolean on, long nowMs) {
        List<Action> out = new ArrayList<Action>();
        interactive = on;
        if (on && expectOnUntil >= nowMs) {
            expectOnUntil = -1;
            return out;
        }
        if (!on && expectOffUntil >= nowMs) {
            expectOffUntil = -1;
            return out;
        }
        if (on && glassd == Frames.SCREEN_OFF) {
            out.add(Action.REQUEST_WAKE);
        } else if (!on && (glassd == Frames.SCREEN_ON || glassd == Frames.SCREEN_DIM)) {
            out.add(Action.REQUEST_SLEEP);
        }
        return out;
    }

    /** The connection to glassd was lost: give the screen back to the framework's policy. */
    public List<Action> onDisconnected() {
        List<Action> out = new ArrayList<Action>();
        glassd = Frames.SCREEN_UNKNOWN;
        expectOnUntil = -1;
        expectOffUntil = -1;
        out.add(Action.LOCK_NONE);
        return out;
    }
}
