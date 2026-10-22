// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/** What the glue needs from Android (GlueService implements it; tests fake it). */
public interface Platform {
    /** A connected stream to glassd's socket. */
    interface Connection {
        InputStream input() throws IOException;

        OutputStream output() throws IOException;

        void close();
    }

    Connection connect() throws IOException;

    void screen(ScreenSync.Action action);

    /** Injects a key press (down and up) into the focused window. */
    void injectKey(int keyCode);

    /** The power key was held: shut down. */
    void shutdown();

    long uptimeMs();

    void log(String message);
}
