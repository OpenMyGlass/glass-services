// SPDX-License-Identifier: Apache-2.0
package org.openmyglass.services.glue;

import java.io.ByteArrayOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.Charset;

/**
 * The protobuf wire format and the varint32 length framing of docs/api.md, for the few
 * messages the glue uses. Hand-written instead of generated (no protoc and no protobuf
 * runtime in the build); CI checks it against a real glassd (GlueTests, end to end).
 */
public final class Wire {
    public static final int MAX_FRAME = 64 * 1024;
    static final Charset UTF8 = Charset.forName("UTF-8");

    static final int VARINT = 0;
    static final int FIXED64 = 1;
    static final int BYTES = 2;
    static final int FIXED32 = 5;

    private Wire() {}

    /** Builds one message. */
    public static final class Writer {
        private final ByteArrayOutputStream out = new ByteArrayOutputStream();

        void varint(long v) {
            while ((v & ~0x7FL) != 0) {
                out.write((int) ((v & 0x7F) | 0x80));
                v >>>= 7;
            }
            out.write((int) v);
        }

        void tag(int field, int type) {
            varint(((long) field << 3) | type);
        }

        public Writer uint(int field, long v) {
            if (v != 0) {
                tag(field, VARINT);
                varint(v);
            }
            return this;
        }

        public Writer bool(int field, boolean v) {
            return uint(field, v ? 1 : 0);
        }

        public Writer string(int field, String s) {
            if (s != null && !s.isEmpty()) {
                bytes(field, s.getBytes(UTF8));
            }
            return this;
        }

        public Writer bytes(int field, byte[] b) {
            tag(field, BYTES);
            varint(b.length);
            out.write(b, 0, b.length);
            return this;
        }

        /** An embedded message, written even when empty (a oneof member that is set). */
        public Writer message(int field, Writer m) {
            return bytes(field, m.toBytes());
        }

        /** A packed repeated enum or uint32. */
        public Writer packed(int field, int[] values) {
            if (values.length == 0) {
                return this;
            }
            Writer w = new Writer();
            for (int v : values) {
                w.varint(v & 0xFFFFFFFFL);
            }
            return bytes(field, w.toBytes());
        }

        public byte[] toBytes() {
            return out.toByteArray();
        }
    }

    /** Reads the fields of one message. */
    public static final class Reader {
        private final byte[] buf;
        private int pos;
        private final int end;
        private int field;
        private int type;

        public Reader(byte[] buf) {
            this(buf, 0, buf.length);
        }

        Reader(byte[] buf, int off, int len) {
            this.buf = buf;
            this.pos = off;
            this.end = off + len;
        }

        /** Advances to the next field; false at the end. */
        public boolean next() throws IOException {
            if (pos >= end) {
                return false;
            }
            long key = varint();
            field = (int) (key >>> 3);
            type = (int) (key & 7);
            if (field == 0) {
                throw new IOException("field number 0");
            }
            return true;
        }

        public int field() {
            return field;
        }

        public int type() {
            return type;
        }

        long varint() throws IOException {
            long v = 0;
            for (int shift = 0; shift < 64; shift += 7) {
                if (pos >= end) {
                    throw new EOFException("truncated varint");
                }
                int b = buf[pos++] & 0xFF;
                v |= (long) (b & 0x7F) << shift;
                if ((b & 0x80) == 0) {
                    return v;
                }
            }
            throw new IOException("varint too long");
        }

        public long uint() throws IOException {
            expect(VARINT);
            return varint();
        }

        public boolean bool() throws IOException {
            return uint() != 0;
        }

        public float fixed32Float() throws IOException {
            expect(FIXED32);
            if (end - pos < 4) {
                throw new EOFException("truncated fixed32");
            }
            int v = (buf[pos] & 0xFF) | (buf[pos + 1] & 0xFF) << 8 | (buf[pos + 2] & 0xFF) << 16
                    | (buf[pos + 3] & 0xFF) << 24;
            pos += 4;
            return Float.intBitsToFloat(v);
        }

        public byte[] bytes() throws IOException {
            expect(BYTES);
            long len = varint();
            if (len < 0 || len > end - pos) {
                throw new EOFException("truncated length-delimited field");
            }
            byte[] b = new byte[(int) len];
            System.arraycopy(buf, pos, b, 0, (int) len);
            pos += (int) len;
            return b;
        }

        public String string() throws IOException {
            return new String(bytes(), UTF8);
        }

        public Reader message() throws IOException {
            return new Reader(bytes());
        }

        /** Skips the current field (unknown fields are ignored, as protobuf does). */
        public void skip() throws IOException {
            switch (type) {
                case VARINT:
                    varint();
                    break;
                case FIXED64:
                    advance(8);
                    break;
                case BYTES:
                    long len = varint();
                    if (len < 0 || len > end - pos) {
                        throw new EOFException("truncated length-delimited field");
                    }
                    pos += (int) len;
                    break;
                case FIXED32:
                    advance(4);
                    break;
                default:
                    throw new IOException("unsupported wire type " + type);
            }
        }

        private void advance(int n) throws IOException {
            if (end - pos < n) {
                throw new EOFException("truncated field");
            }
            pos += n;
        }

        private void expect(int t) throws IOException {
            if (type != t) {
                throw new IOException("field " + field + ": wire type " + type + ", expected " + t);
            }
        }
    }

    /** Writes one frame: the varint32 length, then the message. */
    public static void writeFrame(OutputStream out, byte[] message) throws IOException {
        if (message.length > MAX_FRAME) {
            throw new IOException("frame of " + message.length + " bytes exceeds 64 KiB");
        }
        Writer w = new Writer();
        w.varint(message.length);
        byte[] len = w.toBytes();
        byte[] all = new byte[len.length + message.length];
        System.arraycopy(len, 0, all, 0, len.length);
        System.arraycopy(message, 0, all, len.length, message.length);
        out.write(all);
        out.flush();
    }

    /** Reads one frame; null at a clean end of stream before a frame starts. */
    public static byte[] readFrame(InputStream in) throws IOException {
        long len = 0;
        for (int i = 0; ; i++) {
            int b = in.read();
            if (b < 0) {
                if (i == 0) {
                    return null;
                }
                throw new EOFException("end of stream in a frame length");
            }
            if (i >= 5) {
                throw new IOException("frame length varint longer than 5 bytes");
            }
            len |= (long) (b & 0x7F) << (7 * i);
            if ((b & 0x80) == 0) {
                break;
            }
        }
        if (len > MAX_FRAME) {
            throw new IOException("frame of " + len + " bytes exceeds 64 KiB");
        }
        byte[] msg = new byte[(int) len];
        int got = 0;
        while (got < msg.length) {
            int n = in.read(msg, got, msg.length - got);
            if (n < 0) {
                throw new EOFException("end of stream in a frame");
            }
            got += n;
        }
        return msg;
    }
}
