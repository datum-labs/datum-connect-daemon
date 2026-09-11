package net.datum.imagepoc

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.util.Log
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.FileProvider
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import java.io.File
import java.util.concurrent.TimeUnit

class MainActivity : AppCompatActivity() {

    private lateinit var statusText: TextView
    private lateinit var webView: WebView
    private lateinit var viewButton: Button
    private lateinit var photoFile: File
    private var photoUri: Uri? = null
    // Forced to HTTP/1.1: over HTTP/2 the proxy transparently compresses responses and
    // strips the Content-Encoding header along with Content-Type, so OkHttp has no way
    // to know it needs to decompress. HTTP/1.1 + identity encoding avoids that entirely.
    private val httpClient = OkHttpClient.Builder()
        .protocols(listOf(Protocol.HTTP_1_1))
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(20, TimeUnit.SECONDS)
        .build()

    private val takePicture = registerForActivityResult(ActivityResultContracts.TakePicture()) { success ->
        statusText.text = if (success) "Photo captured — serving it locally" else "Capture cancelled"
    }

    private val requestCamera = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) launchCamera()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        statusText = findViewById(R.id.statusText)
        webView = findViewById(R.id.webView)
        WebView.setWebContentsDebuggingEnabled(true)
        webView.webViewClient = object : WebViewClient() {
            override fun onPageFinished(view: WebView, url: String) {
                Log.d("TunnelWebView", "onPageFinished: $url")
            }
            override fun onReceivedError(view: WebView, errorCode: Int, description: String, failingUrl: String) {
                Log.e("TunnelWebView", "onReceivedError: code=$errorCode desc=$description url=$failingUrl")
            }

            // The datum proxy strips Content-Type in transit, so the WebView's own network
            // stack won't sniff the response as HTML/image and never renders it. Fetch the
            // response ourselves and hand it back with an explicit MIME type instead.
            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
                val url = request.url.toString()
                Log.d("TunnelWebView", "shouldInterceptRequest: mainFrame=${request.isForMainFrame} url=$url")
                return try {
                    val req = Request.Builder().url(url).header("Accept-Encoding", "identity").build()
                val resp = httpClient.newCall(req).execute()
                Log.d("TunnelWebView", "response: url=$url protocol=${resp.protocol} headers=${resp.headers}")
                    val mime = if (url.endsWith("/photo.jpg")) "image/jpeg" else "text/html"
                    WebResourceResponse(mime, null, resp.body?.byteStream())
                } catch (e: Exception) {
                    Log.e("TunnelWebView", "shouldInterceptRequest failed: $url", e)
                    null
                }
            }
        }
        webView.settings.javaScriptEnabled = false

        val photosDir = File(filesDir, "photos").apply { mkdirs() }
        photoFile = File(photosDir, "latest.jpg")

        findViewById<Button>(R.id.captureButton).setOnClickListener {
            requestCamera.launch(Manifest.permission.CAMERA)
        }

        findViewById<Button>(R.id.startTunnelButton).setOnClickListener {
            startForegroundService(Intent(this, TunnelService::class.java))
        }

        viewButton = findViewById(R.id.viewButton)
        viewButton.setOnClickListener {
            TunnelState.hostname.value?.let { host -> webView.loadUrl("https://$host/") }
        }

        lifecycleScope.launch {
            repeatOnLifecycle(Lifecycle.State.STARTED) {
                launch { TunnelState.status.collect { statusText.text = it } }
                launch { TunnelState.hostname.collect { host -> viewButton.isEnabled = host != null } }
            }
        }
    }

    private fun launchCamera() {
        val uri = FileProvider.getUriForFile(this, "net.datum.imagepoc.fileprovider", photoFile)
        photoUri = uri
        takePicture.launch(uri)
    }
}
