package dev.cagents.mobile.android.ui

import android.graphics.Bitmap
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.unit.dp
import dev.cagents.mobile.protocol.Call
import dev.cagents.mobile.protocol.DetailKind
import dev.cagents.mobile.protocol.DetailPage
import dev.cagents.mobile.protocol.MAX_DOWNLOAD_IMAGE_BYTES
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.ByteArrayOutputStream
import java.util.Base64

internal data class TranscriptDetail(val item: String, val kind: DetailKind, val title: String, val path: String? = null)

@Composable
internal fun TranscriptDetailDialog(
    vm: MainViewModel,
    session: String,
    detail: TranscriptDetail,
    onDismiss: () -> Unit,
) {
    val cursors = remember { mutableStateListOf<String?>(null) }
    var index by remember { mutableIntStateOf(0) }
    var attempt by remember { mutableIntStateOf(0) }
    var page by remember { mutableStateOf<DetailPage?>(null) }
    var image by remember { mutableStateOf<Bitmap?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var loading by remember { mutableStateOf(true) }

    LaunchedEffect(index, attempt) {
        page = null
        error = null
        loading = true
        val request = Call.ReadDetail(session, detail.item, detail.kind, detail.path, cursors[index])
        try {
            if (detail.kind == DetailKind.Image) {
                image = loadTranscriptImage(vm, request)
            } else {
                page = vm.readDetail(request)
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            error = e.message ?: "Couldn’t load this detail."
        } finally {
            loading = false
        }
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(detail.title) },
        text = {
            val current = page
            val bitmap = image
            when {
                loading -> CircularProgressIndicator()
                error != null -> Text(error.orEmpty(), color = MaterialTheme.colorScheme.error)
                bitmap != null -> Image(
                    bitmap.asImageBitmap(), detail.title,
                    modifier = Modifier.fillMaxWidth().heightIn(max = 420.dp),
                )
                current != null -> SelectionContainer {
                    when (current.kind) {
                        DetailKind.Output -> if (current.text.isEmpty()) Text("No output") else Code(current.text, preserveTrailingWhitespace = true)
                        DetailKind.Diff -> {
                            val patch = current.patches.joinToString("\n") { it.text }
                            if (patch.isEmpty()) Text("No diff was recorded.") else Code(patch, preserveTrailingWhitespace = true)
                        }
                        else -> Unit
                    }
                }
                else -> Unit
            }
        },
        dismissButton = {
            Row {
                if (index > 0) TextButton(enabled = !loading, onClick = { index-- }) { Text("Previous") }
                page?.next?.let { next ->
                    TextButton(enabled = !loading, onClick = {
                        if (cursors.size == index + 1) {
                            cursors.add(next)
                        } else if (cursors[index + 1] != next) {
                            while (cursors.lastIndex > index) cursors.removeAt(cursors.lastIndex)
                            cursors.add(next)
                        }
                        index++
                    }) { Text("Next") }
                }
                if (error != null) TextButton(onClick = {
                    if (index == 0) {
                        attempt++
                    } else {
                        while (cursors.lastIndex > 0) cursors.removeAt(cursors.lastIndex)
                        index = 0
                    }
                }) { Text("Reload") }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } },
    )
}

private suspend fun loadTranscriptImage(vm: MainViewModel, request: Call.ReadDetail): Bitmap =
    withContext(Dispatchers.IO) {
        val bytes = ByteArrayOutputStream()
        var cursor: String? = null
        do {
            val page = vm.readDetail(request.copy(cursor = cursor))
            // 192 KiB of binary data per server page, independently encoded.
            require(page.base64.isNotEmpty() && page.base64.length <= 256 * 1024) {
                "The computer sent an invalid image page."
            }
            val chunk = Base64.getDecoder().decode(page.base64)
            require(bytes.size() + chunk.size <= MAX_DOWNLOAD_IMAGE_BYTES) {
                "This image is too large to preview."
            }
            bytes.write(chunk)
            cursor = page.next
        } while (cursor != null)

        val encoded = bytes.toByteArray()
        boundedBitmap(encoded, 2048) ?: error("This image can’t be displayed.")
    }
