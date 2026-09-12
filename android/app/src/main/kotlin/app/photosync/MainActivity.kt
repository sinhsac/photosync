package app.photosync

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.ScrollView
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.io.File
import java.util.concurrent.Executors

/**
 * Bring-up screen, not the product UI.
 *
 * §19 specifies two tabs and almost no text. This is deliberately not that: it is
 * the smallest thing that can prove the engine works inside a real app with a real
 * `Context` — which is the only way MediaStore can be exercised at all, since a
 * shell process has no app identity.
 *
 * When §19 is built, this screen goes away.
 */
class MainActivity : AppCompatActivity() {

    private lateinit var log: TextView
    private lateinit var scroll: ScrollView
    private lateinit var code: EditText
    private lateinit var status: TextView

    private val ui = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()

    /**
     * The engine's database. In app-private storage, never in the photo library:
     * §14 says the database holds catalogue and state only, and putting it beside
     * the media would make it look like an asset.
     */
    private val dbPath: String by lazy { File(filesDir, "photosync.db").absolutePath }

    private val poll = object : Runnable {
        override fun run() {
            drain()
            ui.postDelayed(this, 400)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        log = findViewById(R.id.log)
        scroll = findViewById(R.id.scroll)
        code = findViewById(R.id.code)
        status = findViewById(R.id.status)

        findViewById<Button>(R.id.selfCheck).setOnClickListener { onSelfCheck() }
        findViewById<Button>(R.id.scan).setOnClickListener { onScan() }
        findViewById<Button>(R.id.receive).setOnClickListener { onReceive() }
        findViewById<Button>(R.id.send).setOnClickListener { onSend() }

        append(Native.init(this))
        append("db: $dbPath")
        requestMediaPermissions()
        ui.post(poll)
    }

    override fun onDestroy() {
        ui.removeCallbacks(poll)
        worker.shutdownNow()
        super.onDestroy()
    }

    /**
     * Reading the library needs permission; writing our own inserts does not on
     * API 29+ (§10.4). Split by API level because the media permissions were
     * introduced in 33 and `READ_EXTERNAL_STORAGE` stopped working for media.
     */
    private fun requestMediaPermissions() {
        val wanted = if (Build.VERSION.SDK_INT >= 33) {
            arrayOf(Manifest.permission.READ_MEDIA_IMAGES, Manifest.permission.READ_MEDIA_VIDEO)
        } else {
            arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
        }
        val missing = wanted.filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isNotEmpty()) {
            ActivityCompat.requestPermissions(this, missing.toTypedArray(), 1)
        }
    }

    private fun onSelfCheck() = offThread("self check") { Native.selfCheck() }

    private fun onScan() = offThread("scan") { Native.scanLibrary(dbPath) }

    private fun onReceive() {
        offThread("receive") {
            val json = Native.startReceiver(dbPath)
            val obj = JSONObject(json)
            if (obj.optBoolean("ok")) {
                val shown = obj.optString("code")
                ui.post { code.setText(shown.replace(" ", "")) }
                "CODE $shown — enter it on the sending device\naddresses ${obj.optJSONArray("addresses")}"
            } else {
                "failed: ${obj.optString("error")}"
            }
        }
    }

    private fun onSend() {
        val typed = code.text.toString().trim()
        if (typed.length != 6) {
            append("enter the 6-digit code shown on the other device")
            return
        }
        offThread("send") { Native.startSender(dbPath, typed, "") }
    }

    /** Runs work off the UI thread and appends whatever it returns. */
    private fun offThread(what: String, body: () -> String) {
        append("$what...")
        worker.execute {
            val out = try {
                body()
            } catch (e: Throwable) {
                "$what threw: ${e.message}"
            }
            ui.post { append(out) }
        }
    }

    /** Pulls buffered engine output and the current totals. */
    private fun drain() {
        val lines = Native.drainLog()
        if (lines.isNotEmpty()) append(lines.trimEnd())

        val s = try {
            JSONObject(Native.status())
        } catch (e: Exception) {
            return
        }
        status.text = if (s.optBoolean("running")) {
            "running — saved ${s.optLong("items_done")}, " +
                "already there ${s.optLong("items_skipped")}, " +
                "failed ${s.optLong("items_failed")}, " +
                "${s.optLong("bytes_done")} bytes"
        } else if (s.optBoolean("finished")) {
            val err = s.optString("error")
            if (err.isNotEmpty() && err != "null") {
                "stopped: $err"
            } else {
                "done — saved ${s.optLong("items_done")}, " +
                    "already there ${s.optLong("items_skipped")}, " +
                    "failed ${s.optLong("items_failed")}, " +
                    "${s.optLong("bytes_done")} bytes"
            }
        } else {
            "idle"
        }
    }

    private fun append(text: String) {
        if (text.isBlank()) return
        log.append(text)
        log.append("\n")
        scroll.post { scroll.fullScroll(View.FOCUS_DOWN) }
    }
}
