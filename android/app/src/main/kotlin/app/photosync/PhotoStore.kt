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
 *
 * # What MediaStore can and cannot see
 *
 * §1 promises the whole library, and this is the honest boundary of that promise.
 * The two collections queried here are what the system gallery itself shows, so
 * "everything in your gallery" is accurate. Two things are genuinely outside it:
 *
 *  - Directories containing a `.nomedia` file. The media scanner skips them
 *    entirely, so those images are not in MediaStore for anyone, including the
 *    gallery app. Reaching them would mean walking the filesystem and requesting
 *    all-files access, which is a different product.
 *  - Items in the trash, and items another app has left pending. Both are excluded
 *    on purpose: neither is a visible asset.
 */
object PhotoStore {

    /** Where received media goes (§10.4). */
    private const val IMAGE_DIR = "Pictures/PhotoSync"
    private const val VIDEO_DIR = "Movies/PhotoSync"

    /** Sentinel for "start from the beginning" in [enumerate]. */
    const val NO_WATERMARK = -1L

    // -----------------------------------------------------------------------
    // Read side (§10.3)
    // -----------------------------------------------------------------------

    private fun imageCollection(): Uri =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
            MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL)
        else MediaStore.Images.Media.EXTERNAL_CONTENT_URI

    private fun videoCollection(): Uri =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
            MediaStore.Video.Media.getContentUri(MediaStore.VOLUME_EXTERNAL)
        else MediaStore.Video.Media.EXTERNAL_CONTENT_URI

    /**
     * How many images and videos MediaStore reports, as JSON.
     *
     * Exists so the scan can state a total instead of implying one. §1 promises the
     * whole library, and "catalogued 3221" only means something next to "MediaStore
     * has 3221". If the two disagree, something was skipped and the log should say
     * so rather than look successful.
     */
    @JvmStatic
    fun count(ctx: Context): String {
        var images = 0L
        var videos = 0L
        ctx.contentResolver.query(
            imageCollection(),
            arrayOf(MediaStore.MediaColumns._ID),
            null,
            null,
            null,
        )?.use { images = it.count.toLong() }
        ctx.contentResolver.query(
            videoCollection(),
            arrayOf(MediaStore.MediaColumns._ID),
            null,
            null,
            null,
        )?.use { videos = it.count.toLong() }
        return JSONObject()
            .put("images", images)
            .put("videos", videos)
            .put("total", images + videos)
            .toString()
    }

    /**
     * Lists images and videos as JSON, a page at a time.
     *
     * # Why paging is by `_ID` and not by offset
     *
     * The obvious version — sort by date, skip `offset` rows, take `limit` — is
     * wrong, and wrong in the worst way: it silently loses photos.
     *
     * `DATE_MODIFIED` is not unique. A burst of shots, or a bulk file copy, gives
     * many rows the same value, and SQLite makes no promise about the order of rows
     * that tie. Between one page and the next that order can differ, so a row can
     * land on both sides of the boundary (harmless — content dedup catches it) or on
     * neither (a photo that never gets copied, with nothing anywhere saying so).
     *
     * `_ID` is unique and monotonic within the media database, so `_ID > watermark`
     * ordered by `_ID` is a total order with no ties and no gaps. It is also cheaper:
     * offset paging re-reads and discards the first `offset` rows on every page,
     * which on a 100,000-asset library is quadratic. A watermark reads each row once.
     *
     * The two collections are walked independently, hence two watermarks. Pass
     * [NO_WATERMARK] for the first call.
     *
     * The returned `id` is a full content URI, not a bare `_ID`. That matters: the id
     * is used as a *local cache key only* (§11), and a URI is what [openOriginalFd]
     * needs, so keeping them the same string removes a conversion that would
     * otherwise have to guess which table a row came from.
     */
    @JvmStatic
    fun enumerate(ctx: Context, limit: Int, afterImageId: Long, afterVideoId: Long): String {
        val rows = JSONArray()
        var imageWatermark = afterImageId
        var videoWatermark = afterVideoId
        var remaining = limit

        // Images first, then videos, so a caller that stops early still gets whole
        // collections in a predictable order.
        remaining -= collectPage(ctx, isVideo = false, after = imageWatermark, budget = remaining, out = rows) { last ->
            imageWatermark = last
        }
        if (remaining > 0) {
            collectPage(ctx, isVideo = true, after = videoWatermark, budget = remaining, out = rows) { last ->
                videoWatermark = last
            }
        }

        return JSONObject()
            .put("rows", rows)
            .put("afterImageId", imageWatermark)
            .put("afterVideoId", videoWatermark)
            .toString()
    }

    /**
     * Appends up to `budget` rows from one collection, and reports the last `_ID`.
     *
     * Returns how many rows were appended. The cursor is walked lazily and abandoned
     * as soon as the budget is spent, so this never materialises the whole library
     * even though the query itself has no LIMIT — which is deliberate, because the
     * portable ways to express LIMIT to a ContentProvider differ across the API
     * levels this app supports.
     */
    private inline fun collectPage(
        ctx: Context,
        isVideo: Boolean,
        after: Long,
        budget: Int,
        out: JSONArray,
        setWatermark: (Long) -> Unit,
    ): Int {
        if (budget <= 0) return 0

        val collection = if (isVideo) videoCollection() else imageCollection()
        val columns = mutableListOf(
            MediaStore.MediaColumns._ID,
            MediaStore.MediaColumns.DISPLAY_NAME,
            MediaStore.MediaColumns.MIME_TYPE,
            MediaStore.MediaColumns.SIZE,
            MediaStore.MediaColumns.DATE_MODIFIED,
            MediaStore.MediaColumns.WIDTH,
            MediaStore.MediaColumns.HEIGHT,
        )
        // DATE_TAKEN is the real creation time and is what §16 carries. It only
        // exists on these two tables, and it can be null.
        columns.add(if (isVideo) MediaStore.Video.Media.DATE_TAKEN else MediaStore.Images.Media.DATE_TAKEN)
        if (isVideo) columns.add(MediaStore.Video.Media.DURATION)

        val selection = if (after >= 0) "${MediaStore.MediaColumns._ID} > ?" else null
        val args = if (after >= 0) arrayOf(after.toString()) else null

        var added = 0
        ctx.contentResolver.query(
            collection,
            columns.toTypedArray(),
            selection,
            args,
            "${MediaStore.MediaColumns._ID} ASC",
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

            while (added < budget && c.moveToNext()) {
                val id = c.getLong(idCol)
                // Advance the watermark even for a row that is filtered out, or the
                // next page would start before it and loop on it forever.
                setWatermark(id)

                val size = c.getLong(sizeCol)
                // A zero-byte row is never a real asset and would be rejected
                // downstream anyway (§13.2).
                if (size <= 0L) continue

                val uri = ContentUris.withAppendedId(collection, id).toString()

                // DATE_MODIFIED is seconds, DATE_TAKEN is milliseconds. Mixing them
                // up shifts every date by a factor of 1000, which shows up as a
                // receiver timeline stuck in 1970.
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
                added++
            }
        }
        return added
    }

    /**
     * Opens an original read-only and hands over an owned descriptor.
     *
     * `detachFd` transfers ownership to the caller, which is what lets Rust wrap it
     * in a `File`. Kotlin must not close it afterwards; Rust closes it exactly once.
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
     * Normalises a MIME type to one the target collection will accept.
     *
     * MediaStore enforces that a row in `Images` has a MIME under `image/` and a row
     * in `Video` has one under `video/`, and it enforces it on *every* write to that
     * row — so a sender that reports `application/octet-stream` would otherwise make
     * the asset unpublishable after its bytes had already been written.
     *
     * Substituting a plausible type is the right call here: the media type itself
     * comes from the descriptor and is trusted, only the MIME string is suspect, and
     * refusing the asset would lose a photo over a label.
     */
    private fun safeMime(mime: String, isVideo: Boolean): String {
        val prefix = if (isVideo) "video/" else "image/"
        if (mime.startsWith(prefix)) return mime
        return if (isVideo) "video/mp4" else "image/jpeg"
    }

    /**
     * Inserts a row with `IS_PENDING = 1` and returns its URI.
     *
     * Pending means invisible: a crash mid-transfer leaves an unseen row rather than
     * a corrupt gallery entry, which is the whole reason §13.3 insists a partial file
     * can never become an asset.
     *
     * Collection and MIME are decided here and never revisited. They cannot be: a row
     * cannot move from `Images` to `Video`, and a MIME the collection rejects fails
     * the update. Both are known at this point because `ASSET_BEGIN` carries the
     * descriptor and arrives first.
     *
     * The display *name* is still provisional — [publish] sets the real one, which is
     * allowed because it does not change the collection.
     */
    @JvmStatic
    fun createPending(
        ctx: Context,
        provisionalName: String,
        mime: String,
        isVideo: Boolean,
    ): String {
        val resolved = safeMime(mime, isVideo)
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, provisionalName)
            put(MediaStore.MediaColumns.MIME_TYPE, resolved)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                put(MediaStore.MediaColumns.RELATIVE_PATH, if (isVideo) VIDEO_DIR else IMAGE_DIR)
                put(MediaStore.MediaColumns.IS_PENDING, 1)
            }
        }
        val collection = if (isVideo) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
                MediaStore.Video.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
            else MediaStore.Video.Media.EXTERNAL_CONTENT_URI
        } else {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
                MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
            else MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        }

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
     * Clears `IS_PENDING`, making the asset visible, and sets its real metadata.
     *
     * Called only after the whole-file hash matched (§13.3).
     *
     * # Why the date has to go in this same write
     *
     * A first attempt set `DATE_TAKEN` in a second `update()` after clearing
     * `IS_PENDING`. It silently did nothing: publishing makes MediaProvider scan the
     * file, and the scan writes the date columns from EXIF — which, for an image
     * whose EXIF carries no `DateTimeOriginal`, means writing null over whatever was
     * there. The photos landed correctly and the gallery filed every one of them
     * under today, which is exactly what §16 exists to prevent.
     *
     * Included in the publishing write, the value survives. It can still be
     * overridden by real EXIF, which is correct — EXIF is the better source.
     *
     * If the provider rejects `DATE_TAKEN`, the write is retried without it. A photo
     * with the wrong date is a far better outcome than an asset whose bytes verified
     * and then failed to become visible.
     *
     * Deliberately does *not* touch `MIME_TYPE` or `RELATIVE_PATH`: both were fixed
     * at insert time by [createPending], and changing either here is what MediaStore
     * rejects.
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
        val resolved = safeMime(mime, isVideo)
        val takenColumn =
            if (isVideo) MediaStore.Video.Media.DATE_TAKEN else MediaStore.Images.Media.DATE_TAKEN

        // The display name is made to agree with the stored MIME: MediaStore
        // validates the extension against the type, so an incoming `photo` or
        // `photo.bin` for an `image/jpeg` row would be refused.
        val name = withMatchingExtension(displayName, resolved)

        fun baseValues() = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                put(MediaStore.MediaColumns.IS_PENDING, 0)
            }
        }

        val withDate = baseValues().apply {
            if (createdAtMillis > 0) {
                // DATE_TAKEN is milliseconds; DATE_MODIFIED is seconds. Getting this
                // wrong is invisible until the gallery sorts everything into 1970.
                put(takenColumn, createdAtMillis)
                put(MediaStore.MediaColumns.DATE_MODIFIED, createdAtMillis / 1000L)
            }
        }

        try {
            ctx.contentResolver.update(target, withDate, null, null)
        } catch (e: Exception) {
            // Publish anyway, without the date. Losing the timeline position is
            // recoverable; losing the asset is not.
            ctx.contentResolver.update(target, baseValues(), null, null)
        }

        return uri
    }

    /**
     * Ensures a display name carries an extension consistent with `mime`.
     *
     * Leaves an already-agreeing name untouched, so a photo that arrives as
     * `IMG_1234.jpg` keeps exactly that name in the gallery.
     */
    private fun withMatchingExtension(name: String, mime: String): String {
        val wanted = when (mime) {
            "image/jpeg" -> "jpg"
            "image/png" -> "png"
            "image/webp" -> "webp"
            "image/gif" -> "gif"
            "image/heic" -> "heic"
            "image/heif" -> "heif"
            "video/mp4" -> "mp4"
            "video/quicktime" -> "mov"
            else -> null
        } ?: return name

        val dot = name.lastIndexOf('.')
        val ext = if (dot > 0) name.substring(dot + 1).lowercase() else ""
        // jpeg/jpg are the same thing; do not rewrite one into the other.
        if (ext == wanted || (wanted == "jpg" && ext == "jpeg")) return name
        val stem = if (dot > 0) name.substring(0, dot) else name
        return "$stem.$wanted"
    }

    /**
     * Finds an existing pending item created by [createPending] for `provisionalName`.
     *
     * Returns its URI, or an empty string if there is none.
     *
     * # Why this is needed
     *
     * `LibrarySink::staging` is documented to return the same bytes when reopened
     * with the same content key, and resume across a process death depends on it
     * (§13.4). The filesystem implementation gets this for free by naming the temp
     * file after the key. MediaStore does not: a second `insert` produces a second,
     * empty row, while the database still remembers `bytes_received` from the first —
     * so the engine would seek past the end of an empty file and treat the gap as
     * received data.
     *
     * Pending rows are hidden from ordinary queries, so this asks for them
     * explicitly. Before API 29 there is no pending state at all and the row is
     * simply visible, which the same query finds.
     */
    @JvmStatic
    fun findPending(ctx: Context, provisionalName: String, isVideo: Boolean): String {
        val base = if (isVideo) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
                MediaStore.Video.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
            else MediaStore.Video.Media.EXTERNAL_CONTENT_URI
        } else {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
                MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
            else MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        }
        @Suppress("DEPRECATION")
        val collection = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q)
            MediaStore.setIncludePending(base)
        else base

        return try {
            ctx.contentResolver.query(
                collection,
                arrayOf(MediaStore.MediaColumns._ID),
                "${MediaStore.MediaColumns.DISPLAY_NAME} = ?",
                arrayOf(provisionalName),
                null,
            )?.use { c ->
                if (c.moveToFirst()) {
                    ContentUris.withAppendedId(base, c.getLong(0)).toString()
                } else {
                    ""
                }
            } ?: ""
        } catch (e: Exception) {
            // Not finding it is always safe — the caller inserts a fresh one and the
            // transfer starts from zero.
            ""
        }
    }

    /**
     * Deletes a pending item.
     *
     * Used on hash mismatch and on cancel. Nothing visible ever existed, so there is
     * nothing for the user to notice.
     */
    @JvmStatic
    fun abandon(ctx: Context, uri: String) {
        try {
            ctx.contentResolver.delete(Uri.parse(uri), null, null)
        } catch (_: Exception) {
        }
    }
}
