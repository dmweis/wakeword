# Wakeword

Get Picovoice access key from <https://console.picovoice.ai/>
Get OpenAI API key from <https://platform.openai.com/api-keys>

## Zenoh

`z_sub --key "wakeword/telemetry/voice_probability_pretty_print"`  

`z_sub --key "wakeword/event/**"`  

`z_put -k wakeword/control/privacy_mode -v '{ "privacy_mode": false }'`  

## Docs for used libraries

[pv_porcupine](https://docs.rs/pv_porcupine)  
[pv_cobra](https://docs.rs/pv_cobra)  
[pv_recorder](https://docs.rs/pv_recorder)  

## Installation

These libraries make strange assumptions about relative paths of dynamic libraries.  
You can either solve this by building on target or manually providing path to libraries.  

[Cobra libs on github](https://github.com/Picovoice/cobra/tree/main/lib)  

Portability is fixed using an ugly hack that copies the portable libraries onto the target
