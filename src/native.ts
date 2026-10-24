import * as path from 'node:path'
import * as fs from 'node:fs'

export interface NativeBinding {
  extractEmbeddedPreview: (path: string) => { jpeg: Buffer; source: string }
  extractEmbeddedPreviews: (paths: string[]) => Array<{ jpeg: Buffer; source: string }>
  parseBurstMetadata?: (paths: string[]) => Array<{ path: string; dateTimeOriginalMs?: number | null; cameraId?: string | null }>
  parseBurstMetadataOne?: (path: string) => { path: string; dateTimeOriginalMs?: number | null; cameraId?: string | null }
}

let cachedNative: NativeBinding | null | undefined

/**
 * Loads the platform-specific prebuilt binary if present.
 */
export function getNativeBinding(): NativeBinding | null {
  if (cachedNative !== undefined) {
    return cachedNative
  }

  // Allow disabling native via environment variable
  if (process.env.FAST_RAW_PREVIEW_DISABLE_NATIVE === '1') {
    cachedNative = null
    return null
  }

  const { platform, arch } = process
  const filename = `fast-raw-preview.${platform}-${arch}.node`

  const candidateDirs = [
    __dirname,
    path.join(__dirname, '..'),
    path.join(__dirname, '..', 'build', 'Release'),
    path.join(__dirname, '..', 'crates', 'fast-raw-preview-core'),
  ]

  for (const dir of candidateDirs) {
    const candidatePath = path.join(dir, filename)
    if (fs.existsSync(candidatePath)) {
      try {
        // eslint-disable-next-line @typescript-eslint/no-require-imports
        const mod = require(candidatePath)
        cachedNative = mod as NativeBinding
        return cachedNative
      } catch {
        // Continue searching other candidates
      }
    }
  }

  cachedNative = null
  return null
}
