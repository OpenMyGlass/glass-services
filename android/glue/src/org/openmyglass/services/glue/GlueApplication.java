// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

import android.app.Application;
import android.app.Instrumentation;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.PowerManager;
import android.os.SystemClock;
import android.util.Log;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Method;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * The glue's process: a persistent system app (shared user id android.uid.system), started by
 * the framework at boot. Runs {@link Glue} on its own thread with the Android platform below.
 */
public final class GlueApplication extends Application implements Platform {
    static final String TAG = "glass-glue";
    /** glassd's socket in /dev/socket (created by init, docs/api.md). */
    static final String SOCKET = "glassd";
    static final String ACTION_REQUEST_SHUTDOWN = "android.intent.action.ACTION_REQUEST_SHUTDOWN";
    static final String EXTRA_KEY_CONFIRM = "android.intent.extra.KEY_CONFIRM";

    private PowerManager power;
    private PowerManager.WakeLock bright;
    private PowerManager.WakeLock dim;
    private Method wakeUp;
    private Method goToSleep;
    private final ExecutorService keys = Executors.newSingleThreadExecutor();
    /** Screen broadcasts are handled off the main thread: they may write to the socket. */
    private final ExecutorService broadcasts = Executors.newSingleThreadExecutor();
    private Glue glue;

    @Override
    @SuppressWarnings("deprecation")
    public void onCreate() {
        super.onCreate();
        power = (PowerManager) getSystemService(Context.POWER_SERVICE);
        bright = power.newWakeLock(PowerManager.SCREEN_BRIGHT_WAKE_LOCK, "glass-glue:bright");
        dim = power.newWakeLock(PowerManager.SCREEN_DIM_WAKE_LOCK, "glass-glue:dim");
        bright.setReferenceCounted(false);
        dim.setReferenceCounted(false);
        // Hidden framework methods (Android 4.4 and 6.0); DEVICE_POWER comes with the platform
        // signature.
        try {
            wakeUp = PowerManager.class.getMethod("wakeUp", long.class);
            goToSleep = PowerManager.class.getMethod("goToSleep", long.class);
        } catch (NoSuchMethodException e) {
            Log.e(TAG, "PowerManager.wakeUp/goToSleep not found: the screen cannot follow glassd", e);
        }
        glue = new Glue(this, power.isScreenOn());
        IntentFilter f = new IntentFilter();
        f.addAction(Intent.ACTION_SCREEN_ON);
        f.addAction(Intent.ACTION_SCREEN_OFF);
        registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context context, Intent intent) {
                final boolean on = Intent.ACTION_SCREEN_ON.equals(intent.getAction());
                broadcasts.execute(new Runnable() {
                    @Override
                    public void run() {
                        glue.onAndroidScreen(on);
                    }
                });
            }
        }, f);
        Thread t = new Thread(glue, "glassd-client");
        t.setDaemon(true);
        t.start();
    }

    @Override
    public Connection connect() throws IOException {
        final LocalSocket s = new LocalSocket();
        try {
            s.connect(new LocalSocketAddress(SOCKET, LocalSocketAddress.Namespace.RESERVED));
        } catch (IOException e) {
            try {
                s.close();
            } catch (IOException ignored) {
                // nothing to do
            }
            throw e;
        }
        return new Connection() {
            @Override
            public InputStream input() throws IOException {
                return s.getInputStream();
            }

            @Override
            public OutputStream output() throws IOException {
                return s.getOutputStream();
            }

            @Override
            public void close() {
                try {
                    s.shutdownInput();
                } catch (IOException ignored) {
                    // already closed
                }
                try {
                    s.close();
                } catch (IOException ignored) {
                    // already closed
                }
            }
        };
    }

    @Override
    public void screen(ScreenSync.Action action) {
        switch (action) {
            case LOCK_BRIGHT:
                bright.acquire();
                dim.release();
                break;
            case LOCK_DIM:
                dim.acquire();
                bright.release();
                break;
            case LOCK_NONE:
                if (bright.isHeld()) bright.release();
                if (dim.isHeld()) dim.release();
                break;
            case WAKE_UP:
                call(wakeUp);
                break;
            case GO_TO_SLEEP:
                call(goToSleep);
                break;
            default:
                break;
        }
    }

    private void call(Method m) {
        if (m == null) {
            return;
        }
        try {
            m.invoke(power, SystemClock.uptimeMillis());
        } catch (Exception e) {
            Log.e(TAG, "PowerManager." + m.getName() + " failed", e);
        }
    }

    @Override
    public void injectKey(final int keyCode) {
        // Instrumentation blocks until the event is dispatched and must not run on the main
        // thread; INJECT_EVENTS comes with the system uid.
        keys.execute(new Runnable() {
            @Override
            public void run() {
                try {
                    new Instrumentation().sendKeyDownUpSync(keyCode);
                } catch (RuntimeException e) {
                    Log.w(TAG, "key " + keyCode + " not injected: " + e);
                }
            }
        });
    }

    @Override
    public void shutdown() {
        Intent i = new Intent(ACTION_REQUEST_SHUTDOWN);
        i.putExtra(EXTRA_KEY_CONFIRM, false);
        i.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        startActivity(i);
    }

    @Override
    public long uptimeMs() {
        return SystemClock.uptimeMillis();
    }

    @Override
    public void log(String message) {
        Log.i(TAG, message);
    }
}
