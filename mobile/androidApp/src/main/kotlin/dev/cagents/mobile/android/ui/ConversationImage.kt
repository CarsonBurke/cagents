package dev.cagents.mobile.android.ui

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.provider.OpenableColumns
import dev.cagents.mobile.protocol.ImageAttachment
import dev.cagents.mobile.protocol.MAX_UPLOAD_IMAGE_BYTES
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.ByteArrayOutputStream
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

/** Sample before allocating pixels, so the declared source size cannot allocate a full bitmap. */
internal fun boundedBitmap(bytes: ByteArray, edge: Int): Bitmap? {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
    val longest = maxOf(bounds.outWidth, bounds.outHeight)
    if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
    var sample = 1
    while (longest.toLong() > edge.toLong() * sample) sample *= 2
    val options = BitmapFactory.Options().apply { inSampleSize = sample }
    return BitmapFactory.decodeByteArray(bytes, 0, bytes.size, options)
}
