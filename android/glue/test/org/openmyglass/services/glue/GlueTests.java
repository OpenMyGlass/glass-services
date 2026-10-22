// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.channels.Channels;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * JVM tests of the glue (no Android, no emulator): every static method named test* runs.
 * With a glassd binary as the first argument, also the end-to-end tests against glassd --mock.
 *
 *   java -cp out org.openmyglass.services.glue.GlueTests [path/to/glassd]
 */
public final class GlueTests {
    static String glassd;

    public static void main(String[] args) throws Exception {
        glassd = args.length > 0 ? args[0] : null;
        int passed = 0;
        List<String> failed = new ArrayList<>();
        for (Method m : GlueTests.class.getDeclaredMethods()) {
            if (!m.getName().startsWith("test") || !Modifier.isStatic(m.getModifiers())) {
                continue;
            }
            if (m.getName().startsWith("testEndToEnd") && glassd == null) {
                System.out.println("skip " + m.getName() + " (no glassd given)");
                continue;
            }
            try {
                m.invoke(null);
                passed++;
                System.out.println("ok   " + m.getName());
            } catch (java.lang.reflect.InvocationTargetException e) {
                failed.add(m.getName());
                System.out.println("FAIL " + m.getName());
                e.getCause().printStackTrace(System.out);
            }
        }
        System.out.println(passed + " passed, " + failed.size() + " failed " + failed);
        System.exit(failed.isEmpty() ? 0 : 1);
    }

    static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    static void eq(Object want, Object got, String what) {
        if (!java.util.Objects.equals(want, got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    // ---- wire ----

    static void testVarintsAndFieldsRoundTrip() throws IOException {
        byte[] b = new Wire.Writer().uint(1, 300).string(2, "glass").bool(3, true).uint(4, 0)
                .uint(5, 0xFFFFFFFFL).toBytes();
        // 300 = 0xAC 0x02; an absent zero field is not written.
        eq("08ac02" + "1205676c617373" + "1801" + "28ffffffff0f", hex(b), "encoding");
        Wire.Reader r = new Wire.Reader(b);
        check(r.next() && r.field() == 1, "field 1");
        eq(300L, r.uint(), "uint");
        check(r.next() && r.field() == 2, "field 2");
        eq("glass", r.string(), "string");
        check(r.next() && r.field() == 3 && r.bool(), "bool");
        check(r.next() && r.field() == 5, "field 5");
        eq(0xFFFFFFFFL, r.uint(), "uint32 max");
        check(!r.next(), "end");
    }

    static void testHelloMatchesTheProtoEncoding() {
        // ClientFrame{hello: Hello{api_version: 1, client_name: "g"}} as prost encodes it.
        eq("0a050801120167", hex(Frames.hello("g")), "hello");
        // Subscribe{topics: [1, 4, 7]} packed.
        eq("1205" + "0a03010407", hex(Frames.subscribe(1, 4, 7)), "subscribe");
        // An empty oneof member is still written.
        eq("1a00", hex(Frames.acquireFocus()), "acquire focus");
        eq("2a020802", hex(Frames.screenRequest(Frames.ACTION_SLEEP)), "screen request");
    }

    static void testParsesServiceFramesAndSkipsUnknownFields() throws IOException {
        // ScreenState{screen: OFF, reason: "timeout", timestamp_us: 5, off_ms: 9} in field 9,
        // with an unknown field 99 (varint) and an unknown fixed64 field 98 before it.
        Wire.Writer st = new Wire.Writer().uint(1, Frames.SCREEN_OFF).string(2, "timeout").uint(3, 5)
                .uint(5, 9);
        Wire.Writer f = new Wire.Writer().uint(99, 7);
        f.tag(98, Wire.FIXED64);
        for (int i = 0; i < 8; i++) {
            f.varint(0);
        }
        f.message(9, st);
        Frames.Event e = Frames.parse(f.toBytes());
        eq(Frames.Event.Kind.SCREEN_STATE, e.kind, "kind");
        eq(Frames.SCREEN_OFF, e.screen, "screen");
        eq("timeout", e.reason, "reason");
        Frames.Event g = Frames.parse(new Wire.Writer()
                .message(3, new Wire.Writer().uint(1, Frames.GESTURE_SWIPE_DOWN).uint(2, 2)).toBytes());
        eq(Frames.Event.Kind.GESTURE, g.kind, "gesture");
        eq(2, g.fingers, "fingers");
        Frames.Event b = Frames.parse(new Wire.Writer()
                .message(14, new Wire.Writer().uint(1, 2).uint(2, 2).uint(3, 77).bool(4, true)).toBytes());
        check(b.kind == Frames.Event.Kind.BUTTON && b.button == 2 && b.buttonAction == 2 && b.screenOff,
                "button");
        eq(Frames.Event.Kind.OTHER, Frames.parse(new byte[0]).kind, "empty frame");
    }

    static void testFramingLimitsAndTruncation() throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        Wire.writeFrame(out, new byte[] {1, 2, 3});
        Wire.writeFrame(out, new byte[200]);
        InputStream in = new ByteArrayInputStream(out.toByteArray());
        eq(3, Wire.readFrame(in).length, "first frame");
        eq(200, Wire.readFrame(in).length, "second frame (2-byte length)");
        check(Wire.readFrame(in) == null, "clean end");
        check(throwsIo(() -> Wire.writeFrame(new ByteArrayOutputStream(), new byte[Wire.MAX_FRAME + 1])),
                "oversize write");
        // Length 65537 as a varint: 0x81 0x80 0x04.
        check(throwsIo(() -> Wire.readFrame(new ByteArrayInputStream(new byte[] {(byte) 0x81, (byte) 0x80, 4}))),
                "oversize read");
        check(throwsIo(() -> Wire.readFrame(new ByteArrayInputStream(new byte[] {5, 1, 2}))), "truncated");
        byte[] six = {(byte) 0x80, (byte) 0x80, (byte) 0x80, (byte) 0x80, (byte) 0x80, 0};
        check(throwsIo(() -> Wire.readFrame(new ByteArrayInputStream(six))), "6-byte length");
        check(throwsIo(() -> Frames.parse(new byte[] {0x4a, 5, 8})), "truncated message");
    }

    interface Io {
        void run() throws IOException;
    }

    static boolean throwsIo(Io r) {
        try {
            r.run();
            return false;
        } catch (IOException e) {
            return true;
        }
    }

    // ---- screen ----

    static void testScreenFollowsGlassd() {
        ScreenSync s = new ScreenSync(true);
        eq(Arrays.asList(ScreenSync.Action.LOCK_BRIGHT), s.onGlassdScreen(Frames.SCREEN_ON, 0), "on");
        eq(Arrays.asList(ScreenSync.Action.LOCK_DIM), s.onGlassdScreen(Frames.SCREEN_DIM, 10), "dim");
        eq(Arrays.asList(ScreenSync.Action.LOCK_NONE, ScreenSync.Action.GO_TO_SLEEP),
                s.onGlassdScreen(Frames.SCREEN_OFF, 20), "off");
        // The echo of our own goToSleep is not reported back.
        eq(Collections.emptyList(), s.onAndroidScreen(false, 30), "echo off");
        eq(Arrays.asList(ScreenSync.Action.WAKE_UP, ScreenSync.Action.LOCK_BRIGHT),
                s.onGlassdScreen(Frames.SCREEN_ON, 40), "wake");
        eq(Collections.emptyList(), s.onAndroidScreen(true, 50), "echo on");
        // A repeated OFF while already off only drops the locks.
        s.onGlassdScreen(Frames.SCREEN_OFF, 60);
        s.onAndroidScreen(false, 61);
        eq(Arrays.asList(ScreenSync.Action.LOCK_NONE), s.onGlassdScreen(Frames.SCREEN_OFF, 70), "off again");
    }

    static void testFrameworkChangesAreReportedToGlassd() {
        ScreenSync s = new ScreenSync(true);
        s.onGlassdScreen(Frames.SCREEN_ON, 0);
        // Something else turned the screen off (no call of ours).
        eq(Arrays.asList(ScreenSync.Action.REQUEST_SLEEP), s.onAndroidScreen(false, 100), "external off");
        s.onGlassdScreen(Frames.SCREEN_OFF, 110);
        // A wake lock with ACQUIRE_CAUSES_WAKEUP turned it on.
        eq(Arrays.asList(ScreenSync.Action.REQUEST_WAKE), s.onAndroidScreen(true, 200), "external on");
        // The echo window is bounded: a late broadcast counts as external.
        ScreenSync t = new ScreenSync(true);
        t.onGlassdScreen(Frames.SCREEN_OFF, 0);
        t.onGlassdScreen(Frames.SCREEN_ON, 10);
        eq(Arrays.asList(ScreenSync.Action.REQUEST_SLEEP),
                t.onAndroidScreen(false, 10 + ScreenSync.ECHO_MS + 1), "late off is external");
    }

    static void testQuickOffAndOnKeepsTheScreenOn() {
        ScreenSync s = new ScreenSync(true);
        s.onGlassdScreen(Frames.SCREEN_ON, 0);
        s.onGlassdScreen(Frames.SCREEN_OFF, 100);
        s.onGlassdScreen(Frames.SCREEN_ON, 300);
        // Both broadcasts come late, the off one first: both are our echoes.
        eq(Collections.emptyList(), s.onAndroidScreen(false, 400), "late off echo");
        eq(Collections.emptyList(), s.onAndroidScreen(true, 450), "on echo");
        check(s.interactive(), "interactive");
    }

    static void testDisconnectGivesTheScreenBack() {
        ScreenSync s = new ScreenSync(true);
        s.onGlassdScreen(Frames.SCREEN_ON, 0);
        eq(Arrays.asList(ScreenSync.Action.LOCK_NONE), s.onDisconnected(), "disconnect");
        eq(Frames.SCREEN_UNKNOWN, s.glassdScreen(), "unknown");
        // While glassd is gone, framework changes are not reported.
        eq(Collections.emptyList(), s.onAndroidScreen(false, 10), "no glassd");
    }

    static void testKeyMap() {
        eq(KeyMap.KEYCODE_DPAD_CENTER, KeyMap.keyFor(Frames.GESTURE_TAP, 1), "tap");
        eq(KeyMap.KEYCODE_DPAD_RIGHT, KeyMap.keyFor(Frames.GESTURE_SWIPE_FORWARD, 1), "forward");
        eq(KeyMap.KEYCODE_DPAD_LEFT, KeyMap.keyFor(Frames.GESTURE_SWIPE_BACKWARD, 1), "backward");
        eq(KeyMap.KEYCODE_BACK, KeyMap.keyFor(Frames.GESTURE_SWIPE_DOWN, 1), "down");
        eq(KeyMap.KEYCODE_DPAD_UP, KeyMap.keyFor(Frames.GESTURE_SWIPE_UP, 1), "up");
        eq(KeyMap.KEYCODE_MENU, KeyMap.keyFor(Frames.GESTURE_LONG_PRESS, 1), "long press");
        eq(0, KeyMap.keyFor(Frames.GESTURE_TAP, 2), "two-finger tap");
        eq(0, KeyMap.keyFor(0, 1), "unknown");
    }

    // ---- the client against a real glassd --mock ----

    /** Records what the glue asks of the platform. */
    static final class FakePlatform implements Platform {
        final Path socket;
        final List<String> calls = Collections.synchronizedList(new ArrayList<String>());

        FakePlatform(Path socket) {
            this.socket = socket;
        }

        @Override
        public Connection connect() throws IOException {
            final SocketChannel ch = SocketChannel.open(StandardProtocolFamily.UNIX);
            ch.connect(UnixDomainSocketAddress.of(socket));
            final InputStream in = Channels.newInputStream(ch);
            final OutputStream out = Channels.newOutputStream(ch);
            return new Connection() {
                public InputStream input() {
                    return in;
                }

                public OutputStream output() {
                    return out;
                }

                public void close() {
                    try {
                        ch.close();
                    } catch (IOException ignored) {
                        // closed
                    }
                }
            };
        }

        public void screen(ScreenSync.Action a) {
            calls.add(a.name());
        }

        public void injectKey(int keyCode) {
            calls.add("key " + keyCode);
        }

        public void shutdown() {
            calls.add("shutdown");
        }

        public long uptimeMs() {
            return System.nanoTime() / 1_000_000;
        }

        public void log(String m) {
            System.out.println("    glue: " + m);
        }

        boolean await(String call, long ms) throws InterruptedException {
            long end = System.currentTimeMillis() + ms;
            while (System.currentTimeMillis() < end) {
                if (calls.contains(call)) {
                    return true;
                }
                Thread.sleep(10);
            }
            return false;
        }
    }

    static void testEndToEndAgainstGlassdMock() throws Exception {
        Path dir = Files.createTempDirectory("glue-e2e");
        Path sock = dir.resolve("g.sock");
        Process p = new ProcessBuilder(glassd, "--mock", "--socket", sock.toString())
                .redirectErrorStream(true).redirectOutput(ProcessBuilder.Redirect.INHERIT).start();
        OutputStream stdin = p.getOutputStream();
        Glue glue = null;
        try {
            for (int i = 0; i < 500 && !Files.exists(sock); i++) {
                Thread.sleep(10);
            }
            FakePlatform fp = new FakePlatform(sock);
            glue = new Glue(fp, false);
            Thread t = new Thread(glue, "glue");
            t.setDaemon(true);
            t.start();
            // The snapshot (screen on) wakes Android and takes the bright lock.
            check(fp.await("LOCK_BRIGHT", 10000), "bright lock after the snapshot: " + fp.calls);
            check(fp.calls.contains("WAKE_UP"), "wake up from the snapshot: " + fp.calls);
            for (int i = 0; i < 500 && !glue.focused(); i++) {
                Thread.sleep(10);
            }
            check(glue.focused(), "focus");
            mock(stdin, "tap");
            check(fp.await("key " + KeyMap.KEYCODE_DPAD_CENTER, 5000), "tap key: " + fp.calls);
            mock(stdin, "swipe forward");
            check(fp.await("key " + KeyMap.KEYCODE_DPAD_RIGHT, 5000), "forward key: " + fp.calls);
            // A two-finger swipe down sleeps (glassd's, not delivered as a gesture).
            mock(stdin, "swipe down 2");
            check(fp.await("GO_TO_SLEEP", 5000), "sleep: " + fp.calls);
            check(!fp.calls.contains("key " + KeyMap.KEYCODE_BACK), "no back key for 2 fingers");
            // Android reports its screen off (the echo): nothing is sent back.
            glue.onAndroidScreen(false);
            // A tap wakes and is consumed.
            fp.calls.remove("WAKE_UP");
            int keys = count(fp.calls, "key ");
            mock(stdin, "tap");
            check(fp.await("WAKE_UP", 5000), "wake: " + fp.calls);
            Thread.sleep(200);
            eq(keys, count(fp.calls, "key "), "the waking tap is consumed");
            glue.onAndroidScreen(true);
            // Something else turns the screen off: glassd follows (ScreenState OFF comes back).
            fp.calls.remove("LOCK_NONE");
            glue.onAndroidScreen(false);
            check(fp.await("LOCK_NONE", 5000), "glassd followed the framework: " + fp.calls);
            mock(stdin, "power long");
            check(fp.await("shutdown", 5000), "power long press: " + fp.calls);
        } finally {
            if (glue != null) {
                glue.stop();
            }
            p.destroy();
            p.waitFor();
        }
    }

    static int count(List<String> l, String prefix) {
        int n = 0;
        synchronized (l) {
            for (String s : l) {
                if (s.startsWith(prefix)) n++;
            }
        }
        return n;
    }

    static void mock(OutputStream stdin, String line) throws IOException {
        stdin.write((line + "\n").getBytes("UTF-8"));
        stdin.flush();
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) {
            s.append(String.format("%02x", x & 0xFF));
        }
        return s.toString();
    }
}
