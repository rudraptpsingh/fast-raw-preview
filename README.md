# fast-raw-preview

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Node](https://img.shields.io/badge/Node-%3E%3D18-green.svg)](https://nodejs.org/)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

**Ultra-fast embedded JPEG preview and burst EXIF extractor for camera RAW files.**

Extracts full-resolution preview JPEGs and capture timestamps from Canon, Sony, Nikon, Fujifilm, Olympus, Panasonic, and Adobe RAW files in milliseconds—without decoding Bayer sensor arrays or spawning heavy ExifTool processes.

---

## Why fast-raw-preview?

Building photography software, desktop cullers, cloud ingest services, or media asset managers usually requires dealing with camera RAW files:
1. **ExifTool (`exiftool-vendored`):** Accurate, but spawns heavy Perl processes. Scanning 1,000 files takes minutes and spikes memory.
2. **LibRaw / dcraw:** Full sensor debayering engines that are **50× to 100× overkill** when your app just needs the embedded camera preview.
3. **Naive Buffer Readers:** Reading entire 50MB–80MB RAW files into JavaScript Buffers exhausts V8 heap memory and triggers GC freezes.
4. **The Sideways Sony Bug:** Cameras (especially Sony ARW) embed preview JPEGs in sensor orientation with no internal orientation tag, requiring the RAW's IFD0 orientation tag to render upright. Most tools render them sideways.

`fast-raw-preview` solves this:
* **Memory-Mapped & Targeted Seeks:** Reads only the first few KB or TIFF IFDs. Never buffers entire RAW files into memory.
* **10× to 50× Faster:** Powered by a multi-core Rust kernel (`rayon` + `memmap2`) with a zero-dependency pure TypeScript fallback.
* **Sony IFD0 Orientation Resolution:** Automatically extracts and maps IFD0 orientation so portrait previews are never rotated sideways.
* **Canon CR3 ISO-BMFF Box Parser:** Locates preview and thumbnail boxes directly without external tools.

---

## Supported Formats

| Brand | Extension | Parser Strategy |
| :--- | :--- | :--- |
| **Canon CR3** | `.cr3` | ISO BMFF box hierarchy (`moov/uuid/prvw`) |
| **Canon CR2** | `.cr2` | TIFF IFD & SubIFD chain walker |
| **Sony** | `.arw`, `.srf`, `.sr2` | TIFF IFD + IFD0 Orientation (`0x0112`) + PreviewIFD (`0xC51B`) |
| **Nikon** | `.nef`, `.nrw` | TIFF SubIFD pointer walker (`0x014A`) |
| **Fujifilm** | `.raf` | RAF header seek (offset `0x54` / `0x58`) |
| **Adobe DNG** | `.dng` | TIFF IFD & SubIFD preview walker |
| **Olympus** | `.orf` | TIFF IFD pointer walker |
| **Panasonic** | `.rw2` | Custom RW2 TIFF magic (`0x0055`) + embedded preview |
| **Pentax** | `.pef` | TIFF IFD preview walker |
| **Hasselblad** | `.3fr` | TIFF SubIFD chain |
| **Kodak / Minolta / Epson** | `.dcr`, `.kdc`, `.mrw`, `.erf` | TIFF container fallback |

---

## Installation

```bash
npm install fast-raw-preview
```

*(Optional)* Or use the Rust core crate directly in your `Cargo.toml`:
```toml
[dependencies]
fast-raw-preview = { git = "https://github.com/rudraptpsingh/fast-raw-preview", package = "fast-raw-preview" }
```

---

## Usage

### 1. Extract Preview from a RAW File

```typescript
import { extractRawPreview } from 'fast-raw-preview'
import fs from 'node:fs/promises'

const result = await extractRawPreview('DSC01234.ARW')

if (result) {
  console.log(`Extracted ${result.width}x${result.height} preview via ${result.source}`)
  console.log(`EXIF Orientation: ${result.orientation}`)
  
  // Save preview JPEG
  await fs.writeFile('preview.jpg', result.jpeg)
}
```

### 2. High-Throughput Batch Processing

Extract previews across thousands of files concurrently:

```typescript
import { extractRawPreviews } from 'fast-raw-preview'

const files = ['IMG_0001.CR3', 'IMG_0002.CR3', 'DSC_0001.NEF']
const previews = await extractRawPreviews(files, { concurrency: 16 })

for (const preview of previews) {
  if (preview) {
    // Process preview buffer
  }
}
```

### 3. Read Burst Timestamp & Camera Metadata

Extract `DateTimeOriginal` and Camera ID without full EXIF parsing:

```typescript
import { readBurstMetadata } from 'fast-raw-preview'

const meta = await readBurstMetadata('DSC01234.ARW')
console.log(meta.dateTimeOriginalMs) // e.g. 1771264741123 ms
console.log(meta.cameraId)           // e.g. "SONY|ILCE-7RM5|3829104"
```

### 4. In-Memory Buffer Extraction

Works on raw buffers (useful in cloud serverless functions or worker threads):

```typescript
import { extractRawPreview } from 'fast-raw-preview'

const fileBuffer = await fetchRawBuffer()
const preview = await extractRawPreview(fileBuffer)
```

---

## CLI Usage

Run directly with `npx`:

```bash
# Extract preview from a single RAW file
npx fast-raw-preview _DSC1234.ARW -o ./previews

# Batch extract all RAW files in a folder with 16 workers
npx fast-raw-preview /Volumes/SD_CARD/DCIM -o ./output -c 16 --verbose

# Output metadata as JSON
npx fast-raw-preview ./photos --json
```

---

## Benchmarks

Measured on a folder of 1,000 mixed RAW images (Sony A7R V, Canon R5, Nikon Z8):

| Tool | Speed (1,000 RAWs) | Memory Peak (RAM) | Requires External Process? |
| :--- | :--- | :--- | :--- |
| **`fast-raw-preview` (Rust)** | **1.8 seconds** | **~45 MB** | **No** (in-process N-API) |
| **`fast-raw-preview` (Pure JS)** | **7.4 seconds** | **~85 MB** | **No** (pure Node.js) |
| `exiftool-vendored` | 42.6 seconds | ~380 MB | Yes (Perl process) |
| `libraw-sys` | 185.0 seconds | ~1.8 GB | No (full demosaic) |

---

## Architecture

`fast-raw-preview` uses a tiered dual-engine design:
1. **Native Rust Engine (`crates/fast-raw-preview-core`):** Memory-maps headers with `memmap2`, parses box structures and IFD chains with SIMD vectorization, and executes batches across CPU cores using `rayon`.
2. **Pure TypeScript Engine:** Zero external runtime dependencies. Performs targeted FileHandle descriptor seeks (`fs.read`) to read only the necessary IFDs.

If native prebuilt binaries are present on the host OS, the native kernel is automatically selected. If running in restricted or alternative environments, it transparently falls back to the pure TypeScript engine with zero configuration.

---

## License

[MIT](LICENSE) © Rudra Pratap Singh
