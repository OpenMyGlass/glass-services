# Third-party notices

Ship this file, or the notices it holds, with any binary image that contains the code named here.

## quircs (vendored in `vendor/quircs`, linked into `glass-qr-worker`)

quircs 0.10.3, <https://github.com/dignifiedquire/quircs>, a Rust port of quirc. Its crate metadata
says MIT; the licence file it ships is quirc's ISC licence, reproduced here as it stands. Both
permit use, change and redistribution with this notice kept. Changed for OpenMyGlass as
`vendor/quircs/VENDORED.md` says.

```text
Copyright (C)
2010-2012 Daniel Beer <dlbeer@gmail.com>
2020 Friedel Ziegelmayer <me@dignifiedquire.com>

Permission to use, copy, modify, and/or distribute this software for
any purpose with or without fee is hereby granted, provided that the
above copyright notice and this permission notice appear in all
copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL
WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL
DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR
PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

The Rust port's crate metadata gives its licence as MIT, without a licence file of its own. For
the port, by Friedel Ziegelmayer, the MIT terms are these:

```text
MIT License

Copyright (c) 2020 Friedel Ziegelmayer <me@dignifiedquire.com>

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Other crates linked into `glass-qr-worker`

Under MIT OR Apache-2.0, whose MIT terms also ask for their copyright notice to be kept: `num-traits`,
`thiserror`, `zeroize`, `base64`, `libc`. Their notices are in each crate's source as published on
crates.io; collecting them for the whole image is not done yet.
