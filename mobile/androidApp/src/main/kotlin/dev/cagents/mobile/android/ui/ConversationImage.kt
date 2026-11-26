package dev.cagents.mobile.android.ui

import android.content.Context
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import android.provider.OpenableColumns
import dev.cagents.mobile.protocol.ImageAttachment
import dev.cagents.mobile.protocol.MAX_UPLOAD_IMAGE_BYTES
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.nio.ByteBuffer
import java.util.Base64

private const val MAX_PICKED_BYTES = 24 * 1024 * 1024

internal suspend fun prepareImage(context: Context, uri: Uri): ImageAttachment = withContext(Dispatchers.IO) {
    val resolver = context.contentResolver
    val sourceName = resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
        if (it.moveToFirst()) it.getString(0) else null
    }
    val name = sourceName.orEmpty().substringBeforeLast('.').take(60).ifBlank { "Image" } + ".png"
    val bytes = checkNotNull(resolver.openInputStream(uri)).use { stream ->
        val out = ByteArrayOutputStream()
        val buffer = ByteArray(16 * 1024)
        while (true) {
            val read = stream.read(buffer)
            if (read < 0) break
            require(out.size() + read <= MAX_PICKED_BYTES) { "The selected photo is too large." }
            out.write(buffer, 0, read)
        }
        out.toByteArray()
    }
    for (edge in listOf(1024, 512, 256)) {
        val bitmap = boundedBitmap(bytes, edge) ?: error("The selected image can’t be decoded.")
        val out = ByteArrayOutputStream()
        val encoded = try {
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
        } finally {
            bitmap.recycle()
        }
        if (encoded && out.size() <= MAX_UPLOAD_IMAGE_BYTES) {
            return@withContext ImageAttachment(name, Base64.getEncoder().encodeToString(out.toByteArray()))
        }
    }
    error("The selected image can’t be compressed to 512 KiB.")
}

/** Decode at display size and preserve the orientation recorded by the camera. */
internal fun boundedBitmap(bytes: ByteArray, edge: Int): Bitmap? = try {
    ImageDecoder.decodeBitmap(ImageDecoder.createSource(ByteBuffer.wrap(bytes))) { decoder, info, _ ->
        decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
        val longest = maxOf(info.size.width, info.size.height)
        var sample = 1
        while (longest.toLong() > edge.toLong() * sample) sample *= 2
        decoder.setTargetSampleSize(sample)
    }
} catch (_: IOException) {
    null
} catch (_: IllegalArgumentException) {
    null
}
