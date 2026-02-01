use super::*;

use base64::{Engine as _, engine::general_purpose};

// Pre checked base64 Kyber keys, ticket, etc.. for testing
// Also used on other tests outside this module for consistency
pub const PRIVATE_KEY_768_TESTING: &str = "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";

pub const PUBLIC_KEY_768_TESTING: &str = "d7qwr9yBu2K0fpAAsYdTyHRCL+iLaxBf5/F019DFOFbC+HY8PgVOH+y2uicK/5td3lpfzjm302eDDxJ9\
8+O2Cpt9R7yQEWCOVWvH26Z2k2BgXSmsovxqvtu5xVKvLOkpKEZyHpGgiumpSxS+0TE5Dgh2eQBJhmSk\
rysKbFWB0QUCQXAaH7BbQMhftKfIJ0W7ckeNilyZDGh2dVlN7Ohl1Bq0L0wvgvNmprFne3uCYmddE2F1\
Xrh/rwbDZvzKf5WRT+DJStdIOVxSV8c4PHkLnls1/XllgaM/9uNEgMtyWKOBHDmYFqCWnsMMlgoB8IGD\
IJKuDCSJuVNQVBuLGiDDrVye16dJpOS9B0uBt3sntvgy/PKSooVuamq3A8k/LzGcFfw9fIOLccKfFhhI\
cbllN9y1C2kYBHo6Lxxb5yNPmUsIdFAsRkg5cWaHh/gA1uEAXDIzttF2WYJEZfej6Hd+dWAJfowOAEnA\
e0KPz2KZBxocJRhndjFp6gxpapesa1mmd2dhu7aqZptAbmCt5tUyXNsjguGCmbqQ7cMc5zKLQbu/U0K8\
i2SD1pxrMgxa4UkKsTVWeMUKKzufCllPdYiiLmAgIZUO3xxK0vsQ45Cq+9xhlWWG61olnlqyx9EoffQh\
3IAR1HO0twEXqCt+BNU5x2ue3cpgZhwWBLfDBpyRWpeW3eGkbHtLt9OYolQCOMROkLKMEyM1k/pfrtYO\
l6NDYbEKwJY3NdkKaFG5ROs5dbIcO1IZ8kyzMfhyTRYkNqLAW+ISRTl0+MdPaIU6MtVHA2h7fpsVw6Nx\
oaASIdEhNIaOy0NRIFZQILsvXSTM3pcHmYkk6QhgIcN/s/anc7KWNsS/TQsvuzXEwFS4GDsAv+rPG3jA\
vrh/MqMAgMAh2GWI9LBaYgFngrA3U2xNGho69jFG2QtcVgc93kyns1cQuGWSL2lixdDGcwUjTetjegNJ\
Uehiz2ZJ7qadQlNPvJOT1dpOcPWKH/moz6gNOrw2LrRHZAOLdiS6arcmCWm/Xhq8gznKn7k178w0lRat\
nGUspBOZDXMNedYjPvKLmGV01ZhUUcET6wWpsbjLLhdEpfMMICW4HZI9GamENvEEjZkHzjEXJGWSMFnI\
vcxLUaWJW/MOqMmjjaabctRzo7dCJ8dKAN2l03UMkkyjF2s1DKlVSkrK+6Mwnnt7smF1k3BWU0ctt5BG\
U8xjP2Vm4npBPTEhpwUJ6LsfZ4pLANddBDHELDAerLuPcWuVc/hnbVxAIUpYK3ASeWKi1gdfCymQWaI1\
5OTKSwtXGNUh7bObbmkzjRofhRgTLCmnXhgL9pmX1SUcfck1drunDjjAPPsl3oO299e17AV4CMMugYKX\
sJZwl6BekOYG1ygz1DgUB0mDPaFVDucPbokk8DMtLOrKu+Bu9rOa7+MdxwYJ+mMjLeae9+MURrdHwtYh\
vaSdDPA8O1h8sGhck2ewpLR3dGCHeyeJsEws1IiCnRa2cqxA+LxenDJV1WeGjWogd+MiNioBJ+kzpLpe\
AFohLlJG5zmHijRc/O4OYbzvH/NisQARHwX3ScApN7qAjG49j2fwJgcrKCM=";

pub fn get_debug_kem_keypair_768() -> ([u8; PRIVATE_KEY_SIZE], [u8; PUBLIC_KEY_SIZE]) {
    let kem_private_key = general_purpose::STANDARD
        .decode(PRIVATE_KEY_768_TESTING)
        .expect("Failed to decode base64 KEM private key")
        .try_into()
        .expect("Invalid KEM private key size");
    
    let kem_public_key = general_purpose::STANDARD
        .decode(PUBLIC_KEY_768_TESTING)
        .expect("Failed to decode base64 KEM public key")
        .try_into()
        .expect("Invalid KEM public key size");

    (kem_private_key, kem_public_key)
}
