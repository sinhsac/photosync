package app.photosync

import android.content.Context

/**
 * The native engine, as seen from Kotlin.
 *
 * Everything here is a thin declaration. The engine is Rust (`crates/`), and this
 * object exists only so Kotlin has names to call.
 *
 * All calls except [drainLog] and [status] can block and must run off the UI
 * thread. [startReceiver] and [startSender] return quickly by design — they hand
 * the work to a Rust thread — but they still touch the network to bind a socket.
 */
object Native {

    init {
        System.loadLibrary("photosync_android")
    }

    /** Must be called once before anything else. Returns JSON. */
    fun init(ctx: Context): String = nativeInit(ctx.applicationContext)

    /**
     * Proves SQLite linked, migrations run, and the candidate query behaves
     * (§22.3). Cheap enough to run at start-up, and worth it: a linker problem
     * otherwise surfaces much later as an unexplained crash.
     */
    fun selfCheck(): String = nativeSelfCheck()

    /** Catalogues and hashes the photo library (§10.3, §15.3). Blocks. */
    fun scanLibrary(dbPath: String): String = nativeScanLibrary(dbPath)

    /** Starts receiving. Returns JSON with the pairing code (§6). */
    fun startReceiver(dbPath: String): String = nativeStartReceiver(dbPath)

    /** Starts sending. `addr` may be empty to discover the peer (§7). */
    fun startSender(dbPath: String, code: String, addr: String): String =
        nativeStartSender(dbPath, code, addr)

    /** Session status as JSON. Safe to poll from the UI thread. */
    fun status(): String = nativeStatus()

    /** Takes buffered progress lines. Safe to poll from the UI thread. */
    fun drainLog(): String = nativeDrainLog()

    private external fun nativeInit(ctx: Context): String
    private external fun nativeSelfCheck(): String
    private external fun nativeScanLibrary(dbPath: String): String
    private external fun nativeStartReceiver(dbPath: String): String
    private external fun nativeStartSender(dbPath: String, code: String, addr: String): String
    private external fun nativeStatus(): String
    private external fun nativeDrainLog(): String
}
