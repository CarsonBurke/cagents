package dev.cagents.mobile.android.ui

/**
 * Tells which of the camera's frames bring a code into view. The camera
 * reads a code on every frame it stays in view, but pointing at a code is
 * one choice: acted on once, and again only after the camera has looked
 * away from it for [awayMillis]. So a pairing cancelled, declined or failed
 * isn't started again by a camera still pointed at its code.
 */
internal class SeenCodes(private val awayMillis: Long = 1_500) {
    enum class Sighting {
        /** No code, or one already acted on still in view. */
        Nothing,

        /** A code coming into view, to act on. */
        New,

        /** The code acted on last, the first time it is seen again after [resume]. */
        StillThere,
    }

    private var last: String? = null

    /** When the camera last saw [last]; null when it hasn't analyzed a frame since [resume]. */
    private var seenAt: Long? = null

    /** One frame the camera analyzed, with the code it read in it, if any. */
    fun frame(text: String?, nowMillis: Long): Sighting {
        val previous = last
        if (previous != null) {
            val seen = seenAt
            if (text == previous) {
                seenAt = nowMillis
                return if (seen == null) Sighting.StillThere else Sighting.Nothing
            }
            when {
                // The camera only now came back on: the time away starts here.
                seen == null -> seenAt = nowMillis
                nowMillis - seen >= awayMillis -> last = null
            }
        }
        if (text == null) return Sighting.Nothing
        last = text
        seenAt = nowMillis
        return Sighting.New
    }

    /**
     * Scanning starts again, the camera having been off meanwhile. The code
     * acted on last stays ignored until the camera has looked away from it.
     */
    fun resume() {
        seenAt = null
    }
}
