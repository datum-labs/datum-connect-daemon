package net.datum.imagepoc

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import fi.iki.elonen.NanoHTTPD
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONObject
import java.io.File
import java.util.concurrent.TimeUnit

object TunnelState {
    val status = MutableStateFlow("idle")
    val hostname = MutableStateFlow<String?>(null)
}

/**
 * Launches the embedded datum-connect-daemon + datumctl binaries on-device, serves the
 * latest captured photo over a loopback NanoHTTPD server, and drives the daemon's local
 * REST API to create+start a tunnel pointed at that server.
 */
class TunnelService : Service() {

    companion object {
        private const val CHANNEL_ID = "datum_tunnel"
        private const val NOTIF_ID = 1

        // Matches the SA credentials minted for this POC (Phase 0) — see sa-creds.json asset.
        private const val SESSION = "android-tunnel-mnfke2@demos-md21mk.identity.miloapis.com@api.datum.net"
        private const val PROJECT = "demos-md21mk"
        private const val DAEMON_PORT = 47780
        private const val ORIGIN_PORT = 8080

        @Volatile
        private var started = false
    }

    private val scope = CoroutineScope(Dispatchers.IO)
    private var httpServer: NanoHTTPD? = null
    private val client = OkHttpClient.Builder()
        .connectTimeout(5, TimeUnit.SECONDS)
        .readTimeout(15, TimeUnit.SECONDS)
        .build()

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
        startForeground(NOTIF_ID, buildNotification("Starting..."))
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!started) {
            started = true
            scope.launch { runPipeline() }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        httpServer?.stop()
        super.onDestroy()
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(CHANNEL_ID, "Datum Tunnel", NotificationManager.IMPORTANCE_LOW)
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }

    private fun buildNotification(text: String): Notification =
        NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("Datum Image POC")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_menu_upload)
            .setOngoing(true)
            .build()

    private fun updateStatus(text: String) {
        TunnelState.status.value = text
        getSystemService(NotificationManager::class.java).notify(NOTIF_ID, buildNotification(text))
    }

    private fun runPipeline() {
        try {
            val nativeDir = File(applicationInfo.nativeLibraryDir)
            val daemonBin = File(nativeDir, "libdatumdaemon.so")
            val ctlBin = File(nativeDir, "libdatumctl.so")
            val home = filesDir
            val connectDir = File(filesDir, "connect").apply { mkdirs() }
            val saCreds = File(filesDir, "sa-creds.json")
            if (!saCreds.exists()) {
                assets.open("sa-creds.json").use { input ->
                    saCreds.outputStream().use { output -> input.copyTo(output) }
                }
            }

            updateStatus("Starting local photo server...")
            val photoFile = File(File(filesDir, "photos").apply { mkdirs() }, "latest.jpg")
            httpServer = object : NanoHTTPD(ORIGIN_PORT) {
                override fun serve(session: IHTTPSession): Response {
                    Log.d("TunnelOrigin", "request: ${session.method} ${session.uri} headers=${session.headers}")
                    return if (session.uri == "/photo.jpg") {
                        if (photoFile.exists()) {
                            newFixedLengthResponse(Response.Status.OK, "image/jpeg", photoFile.inputStream(), photoFile.length())
                        } else {
                            newFixedLengthResponse(Response.Status.NOT_FOUND, "text/plain", "No photo taken yet.")
                        }
                    } else {
                        val html = if (photoFile.exists()) {
                            "<html><body style='margin:0'><img src='/photo.jpg' style='width:100%'></body></html>"
                        } else {
                            "<html><body>No photo taken yet.</body></html>"
                        }
                        newFixedLengthResponse(Response.Status.OK, "text/html", html)
                    }
                }
            }.apply { start() }

            updateStatus("Logging in via datumctl...")
            val loginProc = ProcessBuilder(
                ctlBin.absolutePath, "login", "--credentials", saCreds.absolutePath, "--hostname", "auth.datum.net"
            ).apply {
                environment()["HOME"] = home.absolutePath
                redirectErrorStream(true)
            }.start()
            val loginOutput = loginProc.inputStream.bufferedReader().readText()
            val loginExit = loginProc.waitFor()
            File(filesDir, "login.log").writeText(loginOutput)
            if (loginExit != 0) {
                updateStatus("Login failed (exit $loginExit) — see login.log")
                return
            }

            updateStatus("Starting daemon...")
            val daemonProc = ProcessBuilder(daemonBin.absolutePath, "--port", DAEMON_PORT.toString()).apply {
                environment()["DATUM_PLUGIN_MODE"] = "1"
                environment()["DATUM_SESSION"] = SESSION
                environment()["DATUM_CREDENTIALS_HELPER"] = ctlBin.absolutePath
                environment()["DATUM_PROJECT"] = PROJECT
                environment()["DATUM_CONNECT_DIR"] = connectDir.absolutePath
                environment()["HOME"] = home.absolutePath
                environment()["SSL_CERT_DIR"] = "/system/etc/security/cacerts"
                redirectErrorStream(true)
                redirectOutput(File(filesDir, "daemon.log"))
            }.start()

            updateStatus("Waiting for daemon setup token...")
            val tokenFile = File(connectDir, "daemon_auth/setup.token")
            var waited = 0
            while (!tokenFile.exists() && waited < 30_000 && daemonProc.isAlive) {
                Thread.sleep(500)
                waited += 500
            }
            if (!tokenFile.exists()) {
                updateStatus("Daemon never produced a setup token — see daemon.log")
                return
            }
            val setupToken = tokenFile.readText().trim()
            val base = "http://127.0.0.1:$DAEMON_PORT"

            // The daemon's own `endpoint` field for a tunnel is always its inspector's
            // ephemeral local port (dies with the process) — the real target we asked for
            // (127.0.0.1:$ORIGIN_PORT) is persisted separately, keyed by tunnel id, under
            // DATUM_CONNECT_DIR, and re-applied to a fresh inspector on daemon restart. So
            // the only reliable way to reuse a tunnel is to remember its id ourselves (this
            // avoids leaving cloud-side orphans every run, which was previously piling up
            // and starving the scheduler) and call /start on that exact id again — never try
            // to identify "the right" tunnel by matching endpoint/label from the list.
            val tunnelIdFile = File(filesDir, "tunnel_id.txt")

            fun startTunnel(id: String): Boolean {
                // The daemon reconciles every previously-created tunnel on startup before its
                // REST listen socket opens, so this can take well over 10s once a project has
                // accumulated several tunnels (each restart logs "reconciled inspector"/"auto-
                // resumed" lines per tunnel). A too-short retry budget here causes a false
                // "not found" and creates a needless new tunnel instead of reusing this one.
                for (attempt in 1..60) {
                    val startReq = Request.Builder()
                        .url("$base/v1/tunnels/$id/start")
                        .addHeader("Authorization", "Bearer $setupToken")
                        .post("".toRequestBody(null))
                        .build()
                    try {
                        client.newCall(startReq).execute().use { resp ->
                            if (resp.code == 404) return false
                            val body = resp.body?.string().orEmpty()
                            if (!resp.isSuccessful) throw RuntimeException("start failed: ${resp.code} $body")
                        }
                        return true
                    } catch (e: java.io.IOException) {
                        Thread.sleep(1000)
                    }
                }
                return false
            }

            var tunnelId = tunnelIdFile.takeIf { it.exists() }?.readText()?.trim()?.ifEmpty { null }
            if (tunnelId != null) {
                updateStatus("Reusing existing tunnel...")
                if (!startTunnel(tunnelId!!)) tunnelId = null
            }

            if (tunnelId == null) {
                updateStatus("Creating tunnel...")
                val createBody = JSONObject().apply {
                    put("label", "android-poc")
                    put("endpoint", "127.0.0.1:$ORIGIN_PORT")
                }.toString().toRequestBody("application/json".toMediaType())
                var lastError: Exception? = null
                for (attempt in 1..90) {
                    if (tunnelId != null) break
                    val createReq = Request.Builder()
                        .url("$base/v1/tunnels")
                        .addHeader("Authorization", "Bearer $setupToken")
                        .post(createBody)
                        .build()
                    try {
                        tunnelId = client.newCall(createReq).execute().use { resp ->
                            val body = resp.body?.string().orEmpty()
                            if (!resp.isSuccessful) throw RuntimeException("create failed: ${resp.code} $body")
                            JSONObject(body).getString("id")
                        }
                        break
                    } catch (e: java.io.IOException) {
                        lastError = e
                        Thread.sleep(500)
                    }
                }
                if (tunnelId == null) throw lastError ?: RuntimeException("could not reach daemon API")
                tunnelIdFile.writeText(tunnelId!!)

                updateStatus("Starting tunnel...")
                if (!startTunnel(tunnelId!!)) throw RuntimeException("could not start newly created tunnel")
            }

            updateStatus("Waiting for public hostname...")
            var host: String? = null
            var pollWaited = 0
            while (host == null && pollWaited < 30_000) {
                val progressReq = Request.Builder()
                    .url("$base/v1/tunnels/$tunnelId/progress")
                    .addHeader("Authorization", "Bearer $setupToken")
                    .get()
                    .build()
                client.newCall(progressReq).execute().use { resp ->
                    if (resp.isSuccessful) {
                        val json = JSONObject(resp.body!!.string())
                        val hostnames = json.optJSONArray("hostnames")
                        if (hostnames != null && hostnames.length() > 0) {
                            host = hostnames.getString(0)
                        }
                    }
                }
                if (host == null) {
                    Thread.sleep(1000)
                    pollWaited += 1000
                }
            }

            if (host != null) {
                TunnelState.hostname.value = host
                updateStatus("Tunnel ready: $host")
            } else {
                updateStatus("Tunnel created but no hostname yet — check daemon.log")
            }
        } catch (e: Exception) {
            updateStatus("Error: ${e.message}")
        }
    }
}
