package dev.cagents.mobile.android.scan

import android.util.Size
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.view.CameraController
import androidx.camera.view.LifecycleCameraController
import androidx.camera.view.PreviewView
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.ReaderException
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.qrcode.QRCodeReader
import java.util.concurrent.Executors

/**
 * The camera's view, reading QR codes from it. Each text read is handed to
 * [onText] on the main thread, as often as the code stays in view.
 * [onUnavailable] is called if the camera can't be opened.
 */
@Composable
fun QrScanner(onText: (String) -> Unit, onUnavailable: () -> Unit, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val currentOnText by rememberUpdatedState(onText)
    val currentOnUnavailable by rememberUpdatedState(onUnavailable)
    val controller = remember {
        LifecycleCameraController(context).apply {
            setEnabledUseCases(CameraController.IMAGE_ANALYSIS)
            // Enough detail for a dense code on a screen across the desk, without decoding full frames.
            imageAnalysisResolutionSelector = ResolutionSelector.Builder()
                .setResolutionStrategy(
                    ResolutionStrategy(Size(1280, 720), ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER),
                )
                .build()
            imageAnalysisBackpressureStrategy = ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST
        }
    }
    DisposableEffect(controller, owner) {
        val analysis = Executors.newSingleThreadExecutor()
        val main = ContextCompat.getMainExecutor(context)
        controller.setImageAnalysisAnalyzer(analysis, QrAnalyzer { text -> main.execute { currentOnText(text) } })
        try {
            controller.bindToLifecycle(owner)
            controller.initializationFuture.addListener({
                runCatching { controller.initializationFuture.get() }.onFailure { currentOnUnavailable() }
            }, main)
        } catch (e: RuntimeException) {
            currentOnUnavailable()
        }
        onDispose {
            controller.unbind()
            controller.clearImageAnalysisAnalyzer()
            analysis.shutdown()
        }
    }
    AndroidView(
        factory = { viewContext ->
            PreviewView(viewContext).apply {
                scaleType = PreviewView.ScaleType.FILL_CENTER
                this.controller = controller
            }
        },
        modifier = modifier,
    )
}

/** Reads a QR code from each frame's luminance, the Y plane of its YUV image. */
private class QrAnalyzer(private val onText: (String) -> Unit) : ImageAnalysis.Analyzer {
    private val reader = QRCodeReader()
    private val hints = mapOf(DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE))
    private var luminance = ByteArray(0)

    override fun analyze(image: ImageProxy) {
        image.use {
            val plane = image.planes[0]
            val stride = plane.rowStride
            val size = stride * image.height
            if (luminance.size != size) luminance = ByteArray(size)
            // The last row may stop short of the stride; the rest stays as padding.
            val buffer = plane.buffer
            buffer.rewind()
            buffer.get(luminance, 0, minOf(buffer.remaining(), size))
            val source = PlanarYUVLuminanceSource(luminance, stride, image.height, 0, 0, image.width, image.height, false)
            val text = try {
                reader.decode(BinaryBitmap(HybridBinarizer(source)), hints).text
            } catch (e: ReaderException) {
                null
            } finally {
                reader.reset()
            }
            if (!text.isNullOrEmpty()) onText(text)
        }
    }
}
