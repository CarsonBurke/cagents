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
        for (t in 200L..5_000L step 100) assertEquals(Sighting.Nothing, seen.frame(code, t))
    }

    @Test
    fun cancellingLeavesTheCodeStillInViewIgnored() {
        val seen = SeenCodes()
        assertEquals(Sighting.New, seen.frame(code, 0))
        // Pairing ran for a minute with the camera off, then was cancelled.
        seen.resume()
        // The camera takes a while to come back, still pointed at the code.
        assertEquals(Sighting.StillThere, seen.frame(code, 62_000))
        for (t in 62_100L..70_000L step 100) assertEquals(Sighting.Nothing, seen.frame(code, t))
    }

    @Test
    fun aCodeIsReadAgainAfterTheCameraLooksAway() {
        val seen = SeenCodes()
        seen.frame(code, 0)
        seen.resume()
        // Frames with no code, starting the time away only once the camera is back.
        assertEquals(Sighting.Nothing, seen.frame(null, 60_000))
        assertEquals(Sighting.Nothing, seen.frame(null, 61_000))
        assertEquals(Sighting.Nothing, seen.frame(code, 61_200), "back within the time away")
        assertEquals(Sighting.Nothing, seen.frame(null, 61_500))
        assertEquals(Sighting.Nothing, seen.frame(null, 62_700))
        assertEquals(Sighting.New, seen.frame(code, 62_800))
    }

    @Test
    fun anotherCodeIsReadAtOnce() {
        val seen = SeenCodes()
        seen.frame(code, 0)
        seen.resume()
        assertEquals(Sighting.New, seen.frame(other, 60_000))
        assertEquals(Sighting.Nothing, seen.frame(other, 60_100))
    }

    @Test
    fun aDecodeMissedNowAndThenDoesNotCountAsLookingAway() {
        val seen = SeenCodes()
        seen.frame(code, 0)
        var t = 0L
        repeat(40) {
            t += 250
            assertEquals(Sighting.Nothing, seen.frame(if (it % 3 == 0) null else code, t))
        }
    }
}
