package dev.cagents.mobile.android

import dev.cagents.mobile.android.ssh.PhoneKey
import dev.cagents.mobile.android.ssh.PhoneKeyUnusable
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertFailsWith

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
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(sealed.copyOf(5), wrapping) }
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(ByteArray(0), wrapping) }
        assertFailsWith<PhoneKeyUnusable> { PhoneKey.unsealSeed(seal(ByteArray(16), wrapping), wrapping) }
    }
}
