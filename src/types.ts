/**
 * Result of extracting an embedded preview from a camera RAW file.
 */
export interface RawPreviewResult {
  /**
   * Embedded JPEG buffer.
   */
  jpeg: Buffer

  /**
   * Format / source identifier: 'tiff-ifd' | 'cr3-prvw' | 'raf-fast' | 'exif'
   */
  source: string

  /**
   * The RAW file's true EXIF Orientation (IFD0 tag 0x0112, 1..8).
   * Crucial for Sony ARW and bodies that do not tag the embedded preview itself.
   * 0 or 1 = upright (no rotation needed).
   */
  orientation: number

  /**
   * Parsed JPEG width in pixels (if available).
   */
  width?: number

  /**
   * Parsed JPEG height in pixels (if available).
   */
  height?: number
}

/**
 * Capture metadata used for photo burst and camera identification.
 */
export interface BurstMetadata {
  path: string

  /**
   * Capture timestamp in milliseconds since Unix epoch (derived from DateTimeOriginal).
   */
  dateTimeOriginalMs: number | null

  /**
   * Standardized Camera ID: `Make|Model|Serial` or `Make|Model`.
   */
  cameraId: string | null
}

/**
 * Options for extraction.
 */
export interface ExtractOptions {
  /**
   * Attempt native acceleration if available. Defaults to true.
   */
  preferNative?: boolean

  /**
   * Extract image dimensions from JPEG SOF markers. Defaults to true.
   */
  computeDimensions?: boolean
}
