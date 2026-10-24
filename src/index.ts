import * as path from 'node:path'
import * as fs from 'node:fs/promises'
import type { RawPreviewResult, BurstMetadata, ExtractOptions } from './types'
import { extractTiffPreview, extractTiffPreviewFromBuffer, TIFF_RAW_EXTENSIONS } from './tiff'
import { extractCr3Preview, extractCr3PreviewFromBuffer } from './cr3'
import { extractRafPreview, extractRafPreviewFromBuffer } from './raf'
import { extractJpegDimensions } from './dimensions'
import { getNativeBinding } from './native'

export * from './types'
export { extractJpegDimensions } from './dimensions'

export const ALL_RAW_EXTENSIONS = new Set([
  ...TIFF_RAW_EXTENSIONS,
  '.cr3',
  '.raf',
  '.raw',
])

/**
 * Checks whether a given path or file extension belongs to a supported camera RAW format.
 */
export function isRawImage(filePath: string): boolean {
  const ext = path.extname(filePath).toLowerCase()
  return ALL_RAW_EXTENSIONS.has(ext)
}

/**
 * Extract embedded JPEG preview from a RAW file or in-memory Buffer.
 * Automatically selects the fastest strategy:
 *   1. Native Rust memory-mapped kernel (if prebuilt addon is available)
 *   2. Pure-JS targeted FileHandle seeks for Canon CR3, Fujifilm RAF, Sony ARW, Nikon NEF, DNG, ORF, etc.
 */
export async function extractRawPreview(
  input: string | Buffer,
  options: ExtractOptions = {}
): Promise<RawPreviewResult | null> {
  const preferNative = options.preferNative ?? true

  // String file path
  if (typeof input === 'string') {
    const ext = path.extname(input).toLowerCase()

    // 1. Try native acceleration if available
    if (preferNative) {
      const native = getNativeBinding()
      if (native) {
        try {
          const res = native.extractEmbeddedPreview(input)
          if (res && res.jpeg && res.jpeg.length > 0) {
            const dims = options.computeDimensions !== false ? extractJpegDimensions(res.jpeg) : null
            return {
              jpeg: res.jpeg,
              source: res.source || 'native-rust',
              orientation: 1, // Native parser normalizes or tags
              width: dims?.width,
              height: dims?.height,
            }
          }
        } catch {
          // Fall through to pure JS implementations
        }
      }
    }

    // 2. Pure JS format-specific fast paths
    if (ext === '.raf') {
      return extractRafPreview(input)
    }

    if (ext === '.cr3') {
      return extractCr3Preview(input)
    }

    if (TIFF_RAW_EXTENSIONS.has(ext)) {
      return extractTiffPreview(input)
    }

    // Attempt CR3 / BMFF container check if extension is ambiguous (.raw)
    const cr3Try = await extractCr3Preview(input)
    if (cr3Try) return cr3Try

    // Fall back to TIFF IFD search
    return extractTiffPreview(input)
  }

  // Buffer input
  if (Buffer.isBuffer(input)) {
    if (input.length < 16) return null

    // Fujifilm RAF
    if (input.subarray(0, 16).toString('ascii') === 'FUJIFILMCCD-RAW ') {
      return extractRafPreviewFromBuffer(input)
    }

    // Canon CR3 (ISO BMFF)
    if (input.subarray(4, 8).toString('latin1') === 'ftyp') {
      return extractCr3PreviewFromBuffer(input)
    }

    // TIFF-based (Sony ARW, Nikon NEF, Canon CR2, DNG, ORF, RW2)
    const byteOrder = input.subarray(0, 2).toString('ascii')
    if (byteOrder === 'II' || byteOrder === 'MM') {
      return extractTiffPreviewFromBuffer(input)
    }
  }

  return null
}

/**
 * Concurrently extracts previews for a list of image paths with bounded worker concurrency.
 */
export async function extractRawPreviews(
  paths: string[],
  options: ExtractOptions & { concurrency?: number } = {}
): Promise<Array<RawPreviewResult | null>> {
  const native = options.preferNative !== false ? getNativeBinding() : null

  // If native batch method is present, execute across all cores with Rayon
  if (native && typeof native.extractEmbeddedPreviews === 'function') {
    try {
      const results = native.extractEmbeddedPreviews(paths)
      return results.map((res) => {
        if (!res || !res.jpeg || res.jpeg.length === 0) return null
        const dims = options.computeDimensions !== false ? extractJpegDimensions(res.jpeg) : null
        return {
          jpeg: res.jpeg,
          source: res.source || 'native-rust-batch',
          orientation: 1,
          width: dims?.width,
          height: dims?.height,
        }
      })
    } catch {
      // Fall through to bounded pool
    }
  }

  const concurrency = Math.max(1, options.concurrency ?? 8)
  const results: Array<RawPreviewResult | null> = new Array(paths.length).fill(null)
  let currentIndex = 0

  const worker = async () => {
    while (currentIndex < paths.length) {
      const idx = currentIndex++
      try {
        results[idx] = await extractRawPreview(paths[idx], options)
      } catch {
        results[idx] = null
      }
    }
  }

  const workers = Array.from({ length: Math.min(concurrency, paths.length) }, () => worker())
  await Promise.all(workers)
  return results
}

/**
 * Read burst metadata (capture timestamp and camera ID) from a photo file.
 */
export async function readBurstMetadata(filePath: string): Promise<BurstMetadata> {
  const native = getNativeBinding()
  if (native && typeof native.parseBurstMetadataOne === 'function') {
    try {
      const row = native.parseBurstMetadataOne(filePath)
      return {
        path: filePath,
        dateTimeOriginalMs: row.dateTimeOriginalMs ?? null,
        cameraId: row.cameraId ?? null,
      }
    } catch {
      // Fallback
    }
  }

  // Fallback using filesystem mtime if EXIF native parser is absent
  try {
    const st = await fs.stat(filePath)
    return {
      path: filePath,
      dateTimeOriginalMs: st.mtimeMs,
      cameraId: null,
    }
  } catch {
    return {
      path: filePath,
      dateTimeOriginalMs: null,
      cameraId: null,
    }
  }
}
