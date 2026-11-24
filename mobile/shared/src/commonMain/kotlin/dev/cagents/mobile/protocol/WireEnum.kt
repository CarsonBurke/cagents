package dev.cagents.mobile.protocol

import kotlinx.serialization.KSerializer
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.PrimitiveSerialDescriptor
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder

/** A string enum of the mobile API. Its [wire] value is what the JSON carries. */
interface WireEnum {
    val wire: String
}

/**
 * Decodes a value this app doesn't know to [unknown] instead of failing, as
 * the API's compatibility rules require: a server may add enum values within a
 * version.
 */
abstract class WireEnumSerializer<T>(
    name: String,
    private val entries: List<T>,
    private val unknown: T,
) : KSerializer<T> where T : Enum<T>, T : WireEnum {
    private val byWire = entries.associateBy { it.wire }

    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("dev.cagents.mobile.$name", PrimitiveKind.STRING)

    override fun deserialize(decoder: Decoder): T = byWire[decoder.decodeString()] ?: unknown

    override fun serialize(encoder: Encoder, value: T) = encoder.encodeString(value.wire)
}
