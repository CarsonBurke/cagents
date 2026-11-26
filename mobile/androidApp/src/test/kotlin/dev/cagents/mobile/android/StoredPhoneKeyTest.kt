package dev.cagents.mobile.android

import dev.cagents.mobile.android.ssh.PhoneKey
import dev.cagents.mobile.android.ssh.PhoneKeyUnusable
import dev.cagents.mobile.android.ssh.keyForConnecting
import dev.cagents.mobile.android.ssh.keyIsBroken
import dev.cagents.mobile.client.ConnectFailure
import java.io.IOException
import java.security.InvalidKeyException
import java.security.ProviderException
import java.security.UnrecoverableKeyException
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.SecretKeySpec
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class StoredPhoneKeyTest {
    private fun aesKey(): SecretKey = KeyGenerator.getInstance("AES").apply { init(256) }.generateKey()

    /** As PhoneKey seals a seed: the GCM IV, then the encrypted seed and its tag. */
    private fun seal(seed: ByteArray, key: SecretKey): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, key) }
        return cipher.iv + cipher.doFinal(seed)
    }

    private val seed = ByteArray(32) { it.toByte() }
    private val wrapping = aesKey()

    @Test
    fun opensASeedSealedWithItsKey() {
        assertContentEquals(seed, PhoneKey.unsealSeed(seal(seed, wrapping), wrapping))
    }

    @Test
    fun aSeedWhoseKeyIsGoneIsUnusable() {
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(seal(seed, wrapping), null) }
        // As when the Keystore lost the key and a new one was made under the same name.
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(seal(seed, wrapping), aesKey()) }
    }

    @Test
    fun aDamagedSeedIsUnusable() {
        val sealed = seal(seed, wrapping)
        sealed[20] = (sealed[20] + 1).toByte()
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(sealed, wrapping) }
        // Cut anywhere: before the IV ends, before the tag ends, or within the seed.
        for (size in listOf(0, 5, 12, 20, 27, 28, 59)) {
            assertFailsWith<PhoneKeyUnusable>("$size bytes") { PhoneKey.unsealSeed(sealed.copyOf(size), wrapping) }
        }
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(sealed + 0, wrapping) }
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(seal(ByteArray(16), wrapping), wrapping) }
    }

    @Test
    fun aFailureThatMayPassIsThrownAsItIs() {
        // A key the cipher refuses for no reason the Keystore names may work another time.
        val refused = SecretKeySpec(ByteArray(7), "AES")
        assertFailsWith<InvalidKeyException> { PhoneKey.unsealSeed(seal(seed, wrapping), refused) }
    }

    /** Stands in for android.security.KeyStoreException, which only an Android device makes. */
    private class KeystoreError(val code: Int) : Exception("Keystore error $code")

    private fun broken(failure: Throwable) = keyIsBroken(failure) { (it as? KeystoreError)?.code }

    private fun <T : Throwable> T.causedBy(cause: Throwable): T = apply { initCause(cause) }

    @Test
    fun onlyAKeyThatIsGoneOrDamagedIsBroken() {
        // As the Keystore reports a key it no longer has, or one it can't read.
        assertTrue(broken(UnrecoverableKeyException("Failed to obtain information about key").causedBy(KeystoreError(6))))
        assertTrue(broken(InvalidKeyException("Keystore operation failed", KeystoreError(7))))
        assertTrue(broken(AEADBadTagException()))
        assertTrue(broken(IllegalStateException("wrapped", AEADBadTagException())))
        assertTrue(broken(PhoneKeyUnusable(IOException())))
    }

    @Test
    fun theKeystoreFailingForNowIsNotBroken() {
        // System error, the Keystore not yet unlocked, KeyMint and Keystore failures, and a busy secure element.
        for (code in listOf(1, 2, 3, 4, 10, 11, 16)) {
            assertFalse(broken(UnrecoverableKeyException("Failed to obtain information about key").causedBy(KeystoreError(code))), "code $code")
            assertFalse(broken(InvalidKeyException("Keystore operation failed", KeystoreError(code))), "code $code")
        }
        assertFalse(broken(ProviderException("Keystore operation failed")))
        assertFalse(broken(IOException("Couldn't read the seed")))
        assertFalse(broken(InvalidKeyException("no reason given")))
    }

    @Test
    fun aCycleOfCausesEnds() {
        val first = IllegalStateException("first")
        val second = IllegalStateException("second", first)
        first.initCause(second)
        assertFalse(broken(first))
    }

    @Test
    fun aKeyThatCantBeUsedStopsConnectingAndOtherFailuresRetry() {
        assertEquals("key", keyForConnecting { "key" })
        val stopped = assertFailsWith<ConnectFailure.Refused> {
            keyForConnecting<String> { throw PhoneKeyUnusable(AEADBadTagException()) }
        }
        assertFalse(stopped.retry)
        assertTrue("Settings" in stopped.message.orEmpty())
        val passing = assertFailsWith<ConnectFailure.Network> {
            keyForConnecting<String> { throw ProviderException("Keystore operation failed") }
        }
        assertTrue(passing.retry)
    }
}
