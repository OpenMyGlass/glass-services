// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

import java.io.IOException;
import java.io.OutputStream;
import java.util.List;

/**
 * The glue as a glassd client: connects (and reconnects), follows ScreenState, injects keys
 * while it holds the input focus, and handles the power key's long press. Android-free, so the
 * whole client runs in JVM tests against a real glassd.
 */
public final class Glue implements Runnable {
    public static final String CLIENT_NAME = "android-glue";
    static final long BACKOFF_MIN_MS = 100;
    static final long BACKOFF_MAX_MS = 5000;

    private final Platform platform;
    private final ScreenSync screen;
    private final Object writeLock = new Object();
    private volatile OutputStream out;
    private volatile Platform.Connection conn;
    private volatile boolean running = true;
    private volatile boolean focused;
    /** A session reached Welcome (the reconnection delay starts again from the minimum). */
    private volatile boolean connected;

    public Glue(Platform platform, boolean interactive) {
        this.platform = platform;
        this.screen = new ScreenSync(interactive);
    }

    public boolean focused() {
        return focused;
    }

    /** Stops the loop (tests). */
    public void stop() {
        running = false;
        Platform.Connection c = conn;
        if (c != null) {
            c.close();
        }
    }

    /**
     * Android's screen broadcast; may be called from any thread except the main one (it may
     * write to the socket). The screen's state and its actions are kept in order by one lock.
     */
    public void onAndroidScreen(boolean on) {
        synchronized (screen) {
            apply(screen.onAndroidScreen(on, platform.uptimeMs()));
        }
    }

    private void apply(List<ScreenSync.Action> actions) {
        for (ScreenSync.Action a : actions) {
            switch (a) {
                case REQUEST_WAKE:
                    send(Frames.screenRequest(Frames.ACTION_WAKE));
                    break;
                case REQUEST_SLEEP:
                    send(Frames.screenRequest(Frames.ACTION_SLEEP));
                    break;
                default:
                    platform.screen(a);
            }
        }
    }

    private boolean send(byte[] frame) {
        synchronized (writeLock) {
            OutputStream o = out;
            if (o == null) {
                return false;
            }
            try {
                Wire.writeFrame(o, frame);
                return true;
            } catch (IOException e) {
                platform.log("send failed: " + e);
                Platform.Connection c = conn;
                if (c != null) {
                    c.close();
                }
                return false;
            }
        }
    }

    @Override
    public void run() {
        long backoff = BACKOFF_MIN_MS;
        while (running) {
            Platform.Connection c;
            try {
                c = platform.connect();
            } catch (IOException e) {
                platform.log("glassd unavailable (" + e.getMessage() + "); retrying in " + backoff + " ms");
                sleep(backoff);
                backoff = Math.min(backoff * 2, BACKOFF_MAX_MS);
                continue;
            }
            conn = c;
            try {
                session(c);
            } catch (IOException e) {
                platform.log("glassd connection lost: " + e.getMessage());
            } finally {
                synchronized (writeLock) {
                    out = null;
                }
                conn = null;
                c.close();
                focused = false;
                synchronized (screen) {
                    apply(screen.onDisconnected());
                }
            }
            if (connected) {
                connected = false;
                backoff = BACKOFF_MIN_MS;
            }
            if (running) {
                sleep(backoff);
                backoff = Math.min(backoff * 2, BACKOFF_MAX_MS);
            }
        }
    }

    private void session(Platform.Connection c) throws IOException {
        java.io.InputStream in = c.input();
        synchronized (writeLock) {
            out = c.output();
            Wire.writeFrame(out, Frames.hello(CLIENT_NAME));
        }
        byte[] first = Wire.readFrame(in);
        if (first == null) {
            throw new IOException("closed before Welcome");
        }
        Frames.Event w = Frames.parse(first);
        if (w.kind != Frames.Event.Kind.WELCOME) {
            throw new IOException("expected Welcome, got " + w);
        }
        platform.log("connected to glassd " + w.serviceVersion + " " + w.capabilities);
        connected = true;
        if (!send(Frames.subscribe(Frames.TOPIC_GESTURE, Frames.TOPIC_SCREEN, Frames.TOPIC_BUTTON))
                || !send(Frames.acquireFocus())) {
            throw new IOException("handshake send failed");
        }
        while (running) {
            byte[] f = Wire.readFrame(in);
            if (f == null) {
                throw new IOException("glassd closed the connection");
            }
            handle(Frames.parse(f));
        }
    }

    void handle(Frames.Event e) {
        switch (e.kind) {
            case SCREEN_STATE: {
                synchronized (screen) {
                    apply(screen.onGlassdScreen(e.screen, platform.uptimeMs()));
                }
                break;
            }
            case FOCUS_STATE:
                focused = e.focused;
                break;
            case GESTURE:
                if (focused) {
                    int key = KeyMap.keyFor(e.gesture, e.fingers);
                    if (key != 0) {
                        platform.injectKey(key);
                    }
                }
                break;
            case BUTTON:
                if (e.button == Frames.BUTTON_POWER && e.buttonAction == Frames.BUTTON_LONG_PRESS) {
                    platform.log("power key held: shutting down");
                    platform.shutdown();
                }
                break;
            case ERROR:
                platform.log("glassd error " + e.errorCode + ": " + e.message);
                break;
            default:
                break;
        }
    }

    private static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }
}
