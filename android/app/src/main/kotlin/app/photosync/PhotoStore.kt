package app.photosync

import android.content.ContentUris
import android.content.ContentValues
import android.content.Context
import android.net.Uri
import android.os.Build
import android.provider.MediaStore
import org.json.JSONArray
import org.json.JSONObject

/**
 * MediaStore access for the native engine (`app_info.md` §10.3, §10.4).
 *
 * Called from Rust through JNI. Every method takes the [Context] explicitly rather
 * than holding static state, so there is nothing to leak and nothing to initialise
 * in the wrong order.
 *
 * The engine knows none of this. It sees `LibrarySource` and `LibrarySink` (§17),
 * and this file is the only place MediaStore appears.
 */
object PhotoStore {

    /** Where received media goes (§10.4). */
    private const val IMAGE_DIR = "Pictures/PhotoSync"
    private const val VIDEO_DIR = "Movies/PhotoSync"

    // -----------------------------------------------------------------------
    // Read side (§10.3)
    // -----------------------------------------------------------------------

    /**
     * Lists images and videos, oldest columns first, as JSON.
     *
     * Paged on purpose. §21 forbids holding the library in memory, and a phone with
     * 100,000 assets would otherwise build one enormous result before any work
     * starts. The caller walks it with [limit] and [offset].
     *
     * The returned `id` is a full content URI, not a bare MediaStore `_ID`. That
     * matters: the id is used as a *local cache key only* (§11), and a URI is what
     * [openOriginalFd] needs, so keeping them the same string removes a conversion
     * that would otherwise have to guess which table a row came from.
     */
    @JvmStatic
    fun enumerate(ctx: Context, limit: Int, offset: Int): String {
        val out = JSONArray()
        var remaining = limit
        var skip = offset

        for (isVideo in listOf(false, true)) {
            if (remaining <= 0) break

            val collection = if (isVideo) {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
                    MediaStore.Video.Media.getContentUri(MediaStore.VOLUME_EXTERNAL)
                else MediaStore.Video.Media.EXTERNAL_CONTENT_URI
            } else {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
                    MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL)
                else MediaStore.Images.Media.EXTERNAL_CONTENT_URI
            }

            val columns = mutableListOf(
                MediaStore.MediaColumns._ID,
                MediaStore.MediaColumns.DISPLAY_NAME,
                MediaStore.MediaColumns.MIME_TYPE,
                MediaStore.MediaColumns.SIZE,
                MediaStore.MediaColumns.DATE_MODIFIED,
                MediaStore.MediaColumns.WIDTH,
                MediaStore.MediaColumns.HEIGHT,
            )
            // DATE_TAKEN is the real creation time and is what §16 wants carried.
            // It only exists on these two tables, and can be null.
            columns.add(if (isVideo) MediaStore.Video.Media.DATE_TAKEN else MediaStore.Images.Media.DATE_TAKEN)
            if (isVideo) columns.add(MediaStore.Video.Media.DURATION)

            ctx.contentResolver.query(
                collection,
                columns.toTypedArray(),
                null,
                null,
                "${MediaStore.MediaColumns.DATE_MODIFIED} DESC",
            )?.use { c ->
                val idCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns._ID)
                val nameCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DISPLAY_NAME)
                val mimeCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.MIME_TYPE)
                val sizeCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.SIZE)
                val modCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DATE_MODIFIED)
                val wCol = c.getColumnIndex(MediaStore.MediaColumns.WIDTH)
                val hCol = c.getColumnIndex(MediaStore.MediaColumns.HEIGHT)
                val takenCol = c.getColumnIndex(
                    if (isVideo) MediaStore.Video.Media.DATE_TAKEN else MediaStore.Images.Media.DATE_TAKEN
                )
                val durCol = if (isVideo) c.getColumnIndex(MediaStore.Video.Media.DURATION) else -1

                while (c.moveToNext()) {
                    if (skip > 0) { skip--; continue }
                    if (remaining <= 0) break

                    val size = c.getLong(sizeCol)
                    // A zero-byte row is never a real asset and would be rejected
                    // downstream anyway (§13.2).
                    if (size <= 0L) continue

                    val id = c.getLong(idCol)
                    val uri = ContentUris.withAppendedId(collection, id).toString()

                    // DATE_MODIFIED is seconds, DATE_TAKEN is milliseconds. Mixing
                    // them up shifts every date by a factor of 1000, which shows up
                    // as a receiver timeline stuck in 1970.
                    val modifiedMs = c.getLong(modCol) * 1000L
                    val takenMs = if (takenCol >= 0 && !c.isNull(takenCol)) c.getLong(takenCol) else modifiedMs

                    val row = JSONObject()
                    row.put("id", uri)
                    row.put("name", c.getString(nameCol) ?: "asset-$id")
                    row.put("mime", c.getString(mimeCol) ?: if (isVideo) "video/mp4" else "image/jpeg")
                    row.put("size", size)
                    row.put("taken", takenMs)
                    row.put("modified", modifiedMs)
                    if (wCol >= 0 && !c.isNull(wCol)) row.put("width", c.getInt(wCol))
                    if (hCol >= 0 && !c.isNull(hCol)) row.put("height", c.getInt(hCol))
                    if (durCol >= 0 && !c.isNull(durCol)) row.put("duration", c.getLong(durCol))
                    row.put("video", isVideo)
                    out.put(row)
                    remaining--
                }
            }
        }
        return out.toString()
    }

    /**
     * Opens an original read-only and hands over an owned descriptor.
     *
     * `detachFd` transfers ownership to the caller, which is what lets Rust wrap it
     * in a `File`. Java must not close it afterwards; Rust closes it exactly once.
     *
     * Returns -1 rather than throwing, because a JNI exception left pending is far
     * more awkward to handle on the Rust side than a sentinel.
     */
    @JvmStatic
    fun openOriginalFd(ctx: Context, uri: String): Int =
        try {
            ctx.contentResolver.openFileDescriptor(Uri.parse(uri), "r")?.detachFd() ?: -1
        } catch (e: Exception) {
            -1
        }

    // -----------------------------------------------------------------------
    // Write side (§10.4)
    // -----------------------------------------------------------------------

    /**
     * Inserts a row with `IS_PENDING = 1` and returns its URI.
     *
     * Pending means invisible: a crash mid-transfer leaves an unseen row rather
     * than a corrupt gallery entry, which is the whole reason §13.3 insists a
     * partial file can never become an asset.
     *
     * The name is provisional — the real one arrives with the descriptor, so
     * [publish] sets it. Inserted as an image; [publish] moves it if it turns out
     * to be video. Doing it this way keeps `staging` callable before `ASSET_BEGIN`
     * is known.
     */
    @JvmStatic
    fun createPending(ctx: Context, provisionalName: String): String {
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, provisionalName)
            put(MediaStore.MediaColumns.MIME_TYPE, "image/jpeg")
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                put(MediaStore.MediaColumns.RELATIVE_PATH, IMAGE_DIR)
                put(MediaStore.MediaColumns.IS_PENDING, 1)
            }
        }
        val collection = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
            MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
        else MediaStore.Images.Media.EXTERNAL_CONTENT_URI

        val uri = ctx.contentResolver.insert(collection, values)
            ?: throw IllegalStateException("MediaStore refused the insert")
        return uri.toString()
    }

    /**
     * Opens a pending item read-write, handing over an owned descriptor.
     *
     * Read-write, not write-only: resume has to seek and truncate (§13.4), which is
     * why `StagingFile` requires random access. The pending item *is* the staging
     * file, so publishing later is a metadata update and never a copy.
     */
    @JvmStatic
    fun openPendingFd(ctx: Context, uri: String): Int =
        try {
            ctx.contentResolver.openFileDescriptor(Uri.parse(uri), "rw")?.detachFd() ?: -1
        } catch (e: Exception) {
            -1
        }

    /**
     * Clears `IS_PENDING`, making the asset visible, and fixes up its metadata.
     *
     * Called only after the whole-file hash matched (§13.3). Sets the real display
     * name, the real MIME type, and the creation date from the source so the
     * receiver's timeline is not all "today" (§16).
     */
    @JvmStatic
    fun publish(
        ctx: Context,
        uri: String,
        displayName: String,
        mime: String,
        createdAtMillis: Long,
        isVideo: Boolean,
    ): String {
        val target = Uri.parse(uri)
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, displayName)
            put(MediaStore.MediaColumns.MIME_TYPE, mime)
            // DATE_TAKEN is milliseconds; DATE_MODIFIED is seconds. Getting this
            // wrong is invisible until the gallery sorts everything into 1970.
            put(MediaStore.MediaColumns.DATE_MODIFIED, createdAtMillis / 1000L)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                put(MediaStore.MediaColumns.RELATIVE_PATH, if (isVideo) VIDEO_DIR else IMAGE_DIR)
                put(MediaStore.MediaColumns.IS_PENDING, 0)
            }
        }
        ctx.contentResolver.update(target, values, null, null)

        // DATE_TAKEN lives on the typed tables rather than MediaColumns, so it is
        // set separately and tolerated if the provider rejects it.
        try {
            val taken = ContentValues().apply {
                put(MediaStore.Images.Media.DATE_TAKEN, createdAtMillis)
            }
            ctx.contentResolver.update(target, taken, null, null)
        } catch (_: Exception) {
        }

        return uri
    }

    /**
     * Deletes a pending item.
     *
     * Used on hash mismatch and on cancel. Nothing visible ever existed, so there
     * is nothing for the user to notice.
     */
    @JvmStatic
    fun abandon(ctx: Context, uri: String) {
        try {
            ctx.contentResolver.delete(Uri.parse(uri), null, null)
        } catch (_: Exception) {
        }
    }
}
