# Bundled theme fonts

These four Latin-subset, normal-style, weight-400 WOFF2 files are vendored
unchanged from Fontsource packages pinned to 5.3.0. Each font has its complete
upstream OFL-1.1 notice beside it. Source URLs are acquisition provenance only:
the application loads local Vite assets and makes no CDN font request.

Approved display roles: Nostromo → Black Ops One; 2001 → Michroma;
Tron → Oxanium; LCARS → Dosis. Body/mono stacks and other themes are unchanged.
Actual adoption also requires a display consumer; token presence is insufficient.
Default and CRT source bytes must stay unchanged. No bold/italic face or non-Latin
coverage is bundled; missing glyphs use the retained system fallback chain.

The original family names are retained. No further subsetting or other binary
modification is performed here. Full licence notices, rather than short labels,
are emitted into dist/assets/<family>-LICENSE.txt by the Vite build.

## Provenance

### Oxanium

- Role: geometric techno; font version: Version 2.000.
- Package: `@fontsource/oxanium@5.3.0`; Latin, regular 400.
- File: `web/src/fonts/oxanium/oxanium-latin-400-normal.woff2` (8816 bytes).
- Source: [@fontsource/oxanium](https://cdn.jsdelivr.net/npm/@fontsource/oxanium@5.3.0/files/oxanium-latin-400-normal.woff2).
- SHA-256: `e77dffc577755217c83aead133969093df038412ee7f85520361b24f22dad377`.
- Licence: [OFL-1.1](https://raw.githubusercontent.com/google/fonts/main/ofl/oxanium/OFL.txt); local `web/src/fonts/oxanium/LICENSE.txt`.
- Licence SHA-256: `fe17c0f2581d71b4e1ea7e636e7f4877c29223e11bb1dd1a871e8c3f2a86336b`.

### Black Ops One

- Role: industrial stencil; font version: Version 1.004; ttfautohint (v1.8.4.7-5d5b).
- Package: `@fontsource/black-ops-one@5.3.0`; Latin, regular 400.
- File: `web/src/fonts/black-ops-one/black-ops-one-latin-400-normal.woff2` (31408 bytes).
- Source: [@fontsource/black-ops-one](https://cdn.jsdelivr.net/npm/@fontsource/black-ops-one@5.3.0/files/black-ops-one-latin-400-normal.woff2).
- SHA-256: `6e4c5eb97c2b5a41ebe33f4462385d551dabef7aa28cae7b3d6b77e63529eee5`.
- Licence: [OFL-1.1](https://raw.githubusercontent.com/google/fonts/main/ofl/blackopsone/OFL.txt); local `web/src/fonts/black-ops-one/LICENSE.txt`.
- Licence SHA-256: `f67ca4a200424868ee8fcd5464d7b35ac1cbf7723f6bfdbd19a8524689fab9a8`.

### Dosis

- Role: rounded display; font version: Version 3.002.
- Package: `@fontsource/dosis@5.3.0`; Latin, regular 400.
- File: `web/src/fonts/dosis/dosis-latin-400-normal.woff2` (16616 bytes).
- Source: [@fontsource/dosis](https://cdn.jsdelivr.net/npm/@fontsource/dosis@5.3.0/files/dosis-latin-400-normal.woff2).
- SHA-256: `613f26d923d84672cacd9e37ffadce6bc265bb02b6125c9e9c3721e905f5215b`.
- Licence: [OFL-1.1](https://raw.githubusercontent.com/google/fonts/main/ofl/dosis/OFL.txt); local `web/src/fonts/dosis/LICENSE.txt`.
- Licence SHA-256: `6dfa3951f0c829518b284608fcc1acb196cba1536b1ae1713a49deccde1ccc53`.

### Michroma

- Role: wide caps / letterspaced; font version: Version 1.100; ttfautohint (v1.8.4.7-5d5b);gftools[0.9.29].
- Package: `@fontsource/michroma@5.3.0`; Latin, regular 400.
- File: `web/src/fonts/michroma/michroma-latin-400-normal.woff2` (17908 bytes).
- Source: [@fontsource/michroma](https://cdn.jsdelivr.net/npm/@fontsource/michroma@5.3.0/files/michroma-latin-400-normal.woff2).
- SHA-256: `754fdb31c9906e82cf9c0ecb1d0b528073818fc2c418a39932c83a34cca027b6`.
- Licence: [OFL-1.1](https://raw.githubusercontent.com/google/fonts/main/ofl/michroma/OFL.txt); local `web/src/fonts/michroma/LICENSE.txt`.
- Licence SHA-256: `33c39ce59a45bfb0dae3280e98bdb46be59d6d68efc62d4dc5fae90a26d2f393`.

## Size gates and limitations

Font payload: 74,748 bytes (73.00 KiB). Four full notices: 17,576 bytes
(17.16 KiB). The checked ceilings are 110,789 bytes for all web/src/fonts
files and 89,698 bytes for emitted WOFF2 assets. The build must emit the exact
four source font hashes and all four full notices; omission cannot pass the gate.
Report total tracked-source growth and full dist growth separately in the PR.
These byte ceilings do not measure network compression or browser font memory.

The manifest.json is machine-readable provenance. Browser evidence must record
computed display and body chains, actual loaded local FontFace entries and
requests, zero other-origin requests, and a blocked-WOFF2 fallback capture.
These checks establish the exercised Chromium fixture only, not cross-platform
font conformance, non-Latin coverage or whole-page accessibility.
