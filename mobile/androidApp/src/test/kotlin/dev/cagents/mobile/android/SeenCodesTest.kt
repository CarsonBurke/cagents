package dev.cagents.mobile.android

import dev.cagents.mobile.android.ui.SeenCodes
import dev.cagents.mobile.android.ui.SeenCodes.Sighting
import kotlin.test.Test
import kotlin.test.assertEquals

class SeenCodesTest {
    private val code = "cagents-mobile://pair?v=1&name=a"
    private val other = "cagents-mobile://pair?v=1&name=b"

    @Test
    fun aCodeInViewIsActedOnOnce() {
        val seen = SeenCodes()
        assertEquals(Sighting.Nothing, seen.frame(null, 0))
        assertEquals(Sighting.New, seen.frame(code, 100))
        for (t in 200L..5_000L step 100) assertEquals(Sighting.Same, seen.frame(code, t))
    }

    @Test
    fun cancellingLeavesTheCodeStillInViewIgnored() {
        val seen = SeenCodes()
        assertEquals(Sighting.New, seen.frame(code, 0))
        // Pairing ran for a minute with the camera off, then was cancelled.
        seen.resume()
        for (t in 62_000L..70_000L step 100) assertEquals(Sighting.StillThere, seen.frame(code, t))
    }

    @Test
    fun aSlowCameraStartIsNotLookingAway() {
        val seen = SeenCodes()
        seen.frame(code, 0)
        seen.resume()
        // Dark, unfocused frames for three seconds while pointed at the code.
        for (t in 60_000L..63_000L step 250) assertEquals(Sighting.Nothing, seen.frame(null, t))
        assertEquals(Sighting.StillThere, seen.frame(code, 63_100))
    }

    @Test
    fun aCodeIsReadAgainAfterTheCameraLooksAway() {
        val seen = SeenCodes()
        seen.frame(code, 0)
        seen.resume()
        assertEquals(Sighting.StillThere, seen.frame(code, 60_000))
        assertEquals(Sighting.Nothing, seen.frame(null, 60_500))
        assertEquals(Sighting.StillThere, seen.frame(code, 61_000), "back within the time away")
        assertEquals(Sighting.Nothing, seen.frame(null, 61_500))
        assertEquals(Sighting.Nothing, seen.frame(null, 62_600))
        assertEquals(Sighting.New, seen.frame(code, 62_700))
        assertEquals(Sighting.Same, seen.frame(code, 62_800))
    }

    @Test
    fun anotherCodeIsReadAtOnce() {
        val seen = SeenCodes()
        seen.frame(code, 0)
        seen.resume()
        assertEquals(Sighting.New, seen.frame(other, 60_000))
        assertEquals(Sighting.Same, seen.frame(other, 60_100))
    }

    @Test
    fun aDecodeMissedNowAndThenDoesNotCountAsLookingAway() {
        val seen = SeenCodes()
        seen.frame(code, 0)
        var t = 0L
        repeat(40) {
            t += 250
            val inView = it % 3 != 0
            assertEquals(if (inView) Sighting.Same else Sighting.Nothing, seen.frame(if (inView) code else null, t))
        }
    }

    @Test
    fun scanningAgainBeforeAnyCodeIgnoresNothing() {
        val seen = SeenCodes()
        seen.resume()
        assertEquals(Sighting.New, seen.frame(code, 0))
    }
}
