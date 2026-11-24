package dev.cagents.mobile.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Check
import androidx.compose.material.icons.automirrored.outlined.HelpOutline
import androidx.compose.material.icons.outlined.PanTool
import androidx.compose.material.icons.outlined.VerifiedUser
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import dev.cagents.mobile.protocol.FieldAnswer
import dev.cagents.mobile.protocol.PendingRequest
import dev.cagents.mobile.protocol.RequestKind

/**
 * Something the conversation waits for the person to decide. Every action
 * carries the same weight: nothing is approved by default, and a phone screen
 * is no place to nudge someone towards approving.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun RequestCard(
    request: PendingRequest,
    onAnswer: (action: String, answers: List<FieldAnswer>, done: () -> Unit) -> Unit,
) {
    val status = LocalStatusColors.current
    // Choice fields hold chosen IDs; free-text fields hold one value.
    val values = remember(request.id) {
        mutableStateMapOf<String, List<String>>().apply {
            request.fields.forEach { field -> if (field.initial.isNotEmpty()) put(field.id, listOf(field.initial)) }
        }
    }
    var sending by remember(request.id) { mutableStateOf<String?>(null) }

    Card(
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    when (request.kind) {
                        RequestKind.Question -> Icons.AutoMirrored.Outlined.HelpOutline
                        RequestKind.Consent -> Icons.Outlined.VerifiedUser
                        else -> Icons.Outlined.PanTool
                    },
                    contentDescription = null,
                    tint = status.attention,
                    modifier = Modifier.size(18.dp),
                )
                Spacer(Modifier.width(8.dp))
                Text(
                    when (request.kind) {
                        RequestKind.Approval -> "Approval needed"
                        RequestKind.Question -> "Question"
                        RequestKind.Consent -> "Consent needed"
                        RequestKind.Unknown -> "Waiting for you"
                    },
                    style = MaterialTheme.typography.labelLarge,
                    color = status.attention,
                )
            }
            if (request.title.isNotBlank()) Text(request.title, style = MaterialTheme.typography.titleMedium)
            if (request.description.isNotBlank()) {
                if (request.kind == RequestKind.Approval) Code(request.description) else Text(request.description, style = MaterialTheme.typography.bodyMedium)
            }

            request.fields.forEach { field ->
                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    if (field.label.isNotBlank()) Text(field.label, style = MaterialTheme.typography.labelLarge)
                    if (field.description.isNotBlank()) {
                        Text(field.description, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                    if (field.choices.isEmpty()) {
                        OutlinedTextField(
                            value = values[field.id]?.firstOrNull().orEmpty(),
                            onValueChange = { values[field.id] = listOf(it) },
                            visualTransformation = if (field.secret) PasswordVisualTransformation() else VisualTransformation.None,
                            modifier = Modifier.fillMaxWidth(),
                            enabled = sending == null,
                        )
                    } else {
                        FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            field.choices.forEach { choice ->
                                val chosen = values[field.id].orEmpty()
                                val selected = choice.id in chosen
                                FilterChip(
                                    selected = selected,
                                    enabled = sending == null,
                                    onClick = {
                                        values[field.id] = when {
                                            !field.multiple -> if (selected) emptyList() else listOf(choice.id)
                                            selected -> chosen - choice.id
                                            else -> chosen + choice.id
                                        }
                                    },
                                    leadingIcon = if (selected) {
                                        { Icon(Icons.Outlined.Check, null, Modifier.size(16.dp)) }
                                    } else {
                                        null
                                    },
                                    label = { Text(choice.label.ifBlank { choice.id }) },
                                )
                            }
                        }
                    }
                }
            }

            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                request.actions.forEach { action ->
                    OutlinedButton(
                        enabled = sending == null,
                        onClick = {
                            sending = action.id
                            val answers = request.fields.mapNotNull { field ->
                                values[field.id]?.filter { it.isNotEmpty() }?.takeIf { it.isNotEmpty() }
                                    ?.let { FieldAnswer(field.id, it) }
                            }
                            onAnswer(action.id, answers) { sending = null }
                        },
                    ) {
                        if (sending == action.id) {
                            CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                            Spacer(Modifier.width(8.dp))
                        }
                        Text(action.label.ifBlank { action.id }.replaceFirstChar { it.uppercase() })
                    }
                }
            }
            request.actions.filter { it.description.isNotBlank() }.forEach { action ->
                Text(
                    "${action.label.ifBlank { action.id }}: ${action.description}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}
