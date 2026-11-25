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
        /** No code. */
        Nothing,

        /** A code coming into view, to act on. */
        New,

        /** The code acted on, still in view. */
        Same,

        /** The code acted on before the last [resume], still in view. */
        StillThere,
    }

    private var last: String? = null

    /**
     * When the camera last saw [last]; null when it hasn't since [resume].
     * Until it has, frames without it don't count as looking away: the
     * first frames of a camera starting up, dark and out of focus, read
     * nothing though it is pointed at the code.
     */
    private var seenAt: Long? = null

    /** Whether [last] was acted on before the last [resume]. */
    private var before = false

    /** One frame the camera analyzed, with the code it read in it, if any. */
    fun frame(text: String?, nowMillis: Long): Sighting {
        val previous = last
        if (previous != null) {
            if (text == previous) {
                seenAt = nowMillis
                return if (before) Sighting.StillThere else Sighting.Same
            }
            val seen = seenAt
            if (seen != null && nowMillis - seen >= awayMillis) {
                last = null
                before = false
            }
        }
        if (text == null) return Sighting.Nothing
        last = text
        seenAt = nowMillis
        before = false
        return Sighting.New
    }

    /**
     * Scanning starts again, the camera having been off meanwhile. The code
     * acted on last stays ignored until the camera has seen it and then
     * looked away from it.
     */
    fun resume() {
        seenAt = null
        before = last != null
    }
}
