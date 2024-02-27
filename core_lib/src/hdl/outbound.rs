use anyhow::anyhow;
use hmac::{Hmac, Mac};
use libaes::{Cipher, AES_256_KEY_LEN};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, PublicKey};
use prost::Message;
use rand::Rng;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::broadcast::{Receiver, Sender};

use super::{InnerState, State};
use crate::channel::{ChannelAction, ChannelDirection, ChannelMessage};
use crate::location_nearby_connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
use crate::location_nearby_connections::payload_transfer_frame::{
    payload_header, PacketType, PayloadChunk, PayloadHeader,
};
use crate::location_nearby_connections::{KeepAliveFrame, OfflineFrame, PayloadTransferFrame};
use crate::securegcm::ukey2_alert::AlertType;
use crate::securegcm::ukey2_client_init::CipherCommitment;
use crate::securegcm::{
    ukey2_message, DeviceToDeviceMessage, GcmMetadata, Type, Ukey2Alert, Ukey2ClientFinished,
    Ukey2ClientInit, Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit,
};
use crate::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType,
    SecureMessage, SigScheme,
};
use crate::utils::{
    gen_ecdsa_keypair, gen_random, hkdf_extract_expand, stream_read_exact, to_four_digit_string,
    DeviceType, RemoteDeviceInfo,
};
use crate::{location_nearby_connections, sharing_nearby};

type HmacSha256 = Hmac<Sha256>;

const SANE_FRAME_LENGTH: i32 = 5 * 1024 * 1024;

#[derive(Debug)]
pub enum OutboundPayload {
    File(String),
}

#[derive(Debug)]
pub struct OutboundRequest {
    endpoint_id: [u8; 4],
    socket: TcpStream,
    state: InnerState,
    sender: Sender<ChannelMessage>,
    receiver: Receiver<ChannelMessage>,
    payload: OutboundPayload,
}

impl OutboundRequest {
    pub fn new(
        endpoint_id: [u8; 4],
        socket: TcpStream,
        id: String,
        sender: Sender<ChannelMessage>,
        payload: OutboundPayload,
    ) -> Self {
        let receiver = sender.subscribe();

        Self {
            endpoint_id,
            socket,
            state: InnerState {
                id,
                server_seq: 0,
                client_seq: 0,
                state: State::Initial,
                encryption_done: true,
                text_payload_id: -1,
                ..Default::default()
            },
            sender,
            receiver,
            payload,
        }
    }

    pub async fn handle(&mut self) -> Result<(), anyhow::Error> {
        // Buffer for the 4-byte length
        let mut length_buf = [0u8; 4];

        tokio::select! {
            i = self.receiver.recv() => {
                match i {
                    Ok(channel_msg) => {
                        if channel_msg.direction == ChannelDirection::LibToFront {
                            return Ok(());
                        }

                        if channel_msg.id != self.state.id {
                            return Ok(());
                        }

                        debug!("inbound: got: {:?}", channel_msg);
                        match channel_msg.action {
                            Some(ChannelAction::CancelTransfer) => {
                                todo!()
                            },
                            None => {
                                trace!("inbound: nothing to do")
                            },
                            _ => {}
                        }
                    }
                    Err(e) => {
                        error!("inbound: channel error: {}", e);
                    }
                }
            },
            h = stream_read_exact(&mut self.socket, &mut length_buf) => {
                h?;

                self._handle(length_buf).await?
            }
        }

        Ok(())
    }

    pub async fn _handle(&mut self, length_buf: [u8; 4]) -> Result<(), anyhow::Error> {
        let msg_length = u32::from_be_bytes(length_buf) as usize;
        // Ensure the message length is not unreasonably big to avoid allocation attacks
        if msg_length > SANE_FRAME_LENGTH as usize {
            error!("Message length too big");
            return Err(anyhow!("value"));
        }

        // Allocate buffer for the actual message and read it
        let mut frame_data = vec![0u8; msg_length];
        stream_read_exact(&mut self.socket, &mut frame_data).await?;

        let current_state = &self.state;
        // Now determine what will be the request type based on current state
        match current_state.state {
            State::SentUkeyClientInit => {
                debug!("Handling State::SentUkeyClientInit frame");
                let msg = Ukey2Message::decode(&*frame_data)?;
                self.process_ukey2_server_init(&msg).await?;

                // Advance current state
                self.update_state(
                    |e: &mut InnerState| {
                        e.state = State::SentUkeyClientFinish;
                        e.server_init_data = Some(frame_data);
                        e.encryption_done = true;
                    },
                    false,
                );
            }
            State::SentUkeyClientFinish => {
                debug!("Handling State::SentUkeyClientFinish frame");
            }
            _ => {
                debug!("Handling SecureMessage frame");
                let smsg = SecureMessage::decode(&*frame_data)?;
                self.decrypt_and_process_secure_message(&smsg).await?;
            }
        }

        Ok(())
    }

    pub async fn send_connection_request(&mut self) -> Result<(), anyhow::Error> {
        let request = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionRequest.into(),
                ),
                connection_request: Some(location_nearby_connections::ConnectionRequestFrame {
                    endpoint_id: Some(unsafe {
                        String::from_utf8_unchecked(self.endpoint_id.to_vec())
                    }),
                    endpoint_name: Some(sys_metrics::host::get_hostname()?),
                    endpoint_info: Some(
                        RemoteDeviceInfo {
                            name: sys_metrics::host::get_hostname()?,
                            device_type: DeviceType::Laptop,
                        }
                        .serialize(),
                    ),
                    mediums: vec![Medium::WifiLan.into()],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        self.send_frame(request.encode_to_vec()).await?;

        Ok(())
    }

    pub async fn send_ukey2_client_init(&mut self) -> Result<(), anyhow::Error> {
        let (secret_key, public_key) = gen_ecdsa_keypair();

        let encoded_point = public_key.to_encoded_point(false);
        let x = encoded_point.x().unwrap().to_vec();
        let y = encoded_point.y().unwrap().to_vec();

        let pkey = GenericPublicKey {
            r#type: PublicKeyType::EcP256.into(),
            ec_p256_public_key: Some(EcP256PublicKey { x, y }),
            dh2048_public_key: None,
            rsa2048_public_key: None,
        };

        let finish_frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientFinish.into()),
            message_data: Some(
                Ukey2ClientFinished {
                    public_key: Some(pkey.encode_to_vec()),
                }
                .encode_to_vec(),
            ),
        };

        let sha512 = Sha512::digest(finish_frame.encode_to_vec());
        let frame = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientInit.into()),
            message_data: Some(
                Ukey2ClientInit {
                    version: Some(1),
                    random: Some(gen_random(32)),
                    next_protocol: Some(String::from("AES_256_CBC-HMAC_SHA256")),
                    cipher_commitments: vec![CipherCommitment {
                        handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512.into()),
                        commitment: Some(sha512.to_vec()),
                    }],
                }
                .encode_to_vec(),
            ),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        self.update_state(
            |e| {
                e.state = State::SentUkeyClientInit;
                e.private_key = Some(secret_key);
                e.public_key = Some(public_key);
                e.client_init_msg_data = Some(frame.encode_to_vec());
                e.ukey_client_finish_msg_data = Some(finish_frame.encode_to_vec());
            },
            false,
        );

        Ok(())
    }

    async fn process_ukey2_server_init(&mut self, msg: &Ukey2Message) -> Result<(), anyhow::Error> {
        if msg.message_type() != ukey2_message::Type::ServerInit {
            self.send_ukey2_alert(AlertType::BadMessageType).await?;
            return Err(anyhow!(
                "UKey2: message_type({:?}) != ServerInit",
                msg.message_type
            ));
        }

        let server_init = match Ukey2ServerInit::decode(msg.message_data()) {
            Ok(uk2si) => uk2si,
            Err(e) => {
                return Err(anyhow!("UKey2: Ukey2ClientFinished::decode: {}", e));
            }
        };

        if server_init.version() != 1 {
            self.send_ukey2_alert(AlertType::BadVersion).await?;
            return Err(anyhow!("UKey2: server_init.version != 1"));
        }

        if server_init.random().len() != 32 {
            self.send_ukey2_alert(AlertType::BadRandom).await?;
            return Err(anyhow!("UKey2: server_init.random.len != 32"));
        }

        if server_init.handshake_cipher() != Ukey2HandshakeCipher::P256Sha512 {
            self.send_ukey2_alert(AlertType::BadHandshakeCipher).await?;
            return Err(anyhow!("UKey2: handshake_cipher != P256Sha512"));
        }

        let server_public_key = match GenericPublicKey::decode(server_init.public_key()) {
            Ok(spk) => spk,
            Err(e) => {
                return Err(anyhow!("UKey2: GenericPublicKey::decode: {}", e));
            }
        };

        self.finalize_key_exchange(server_public_key)?;
        self.send_frame(self.state.ukey_client_finish_msg_data.clone().unwrap())
            .await?;

        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::ConnectionResponse.into(),
                ),
                connection_response: Some(location_nearby_connections::ConnectionResponseFrame {
					response: Some(location_nearby_connections::connection_response_frame::ResponseStatus::Accept.into()),
					os_info: Some(location_nearby_connections::OsInfo {
						r#type: Some(location_nearby_connections::os_info::OsType::Linux.into())
					}),
					..Default::default()
				}),
                ..Default::default()
            }),
        };

        self.send_frame(frame.encode_to_vec()).await?;

        Ok(())
    }

    async fn decrypt_and_process_secure_message(
        &mut self,
        smsg: &SecureMessage,
    ) -> Result<(), anyhow::Error> {
        let mut hmac = HmacSha256::new_from_slice(self.state.recv_hmac_key.as_ref().unwrap())?;
        hmac.update(&smsg.header_and_body);
        if !hmac
            .finalize()
            .into_bytes()
            .as_slice()
            .eq(smsg.signature.as_slice())
        {
            return Err(anyhow!("hmac!=signature"));
        }

        let header_and_body = HeaderAndBody::decode(&*smsg.header_and_body)?;

        let msg_data = header_and_body.body;
        let key = self.state.decrypt_key.as_ref().unwrap();

        let mut cipher = Cipher::new_256(key[..AES_256_KEY_LEN].try_into()?);
        cipher.set_auto_padding(true);
        let decrypted = cipher.cbc_decrypt(header_and_body.header.iv(), &msg_data);

        let d2d_msg = DeviceToDeviceMessage::decode(&*decrypted)?;

        let seq = self.get_client_seq_inc();
        if d2d_msg.sequence_number() != seq {
            return Err(anyhow!(
                "Error d2d_msg.sequence_number invalid ({} vs {})",
                d2d_msg.sequence_number(),
                seq
            ));
        }

        let offline = location_nearby_connections::OfflineFrame::decode(d2d_msg.message())?;
        let v1_frame = offline
            .v1
            .as_ref()
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        Ok(())
    }

    async fn disconnection(&mut self) -> Result<(), anyhow::Error> {
        let frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::Disconnection.into(),
                ),
                disconnection: Some(location_nearby_connections::DisconnectionFrame {
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&frame).await
        } else {
            self.send_frame(frame.encode_to_vec()).await
        }
    }

    fn finalize_key_exchange(
        &mut self,
        raw_peer_key: GenericPublicKey,
    ) -> Result<(), anyhow::Error> {
        let peer_p256_key = raw_peer_key
            .ec_p256_public_key
            .ok_or_else(|| anyhow!("Missing required fields"))?;

        let mut bytes = vec![0x04];
        // Ensure no more than 32 bytes for the keys
        if peer_p256_key.x.len() > 32 {
            bytes.extend_from_slice(&peer_p256_key.x[peer_p256_key.x.len() - 32..]);
        } else {
            bytes.extend_from_slice(&peer_p256_key.x);
        }
        if peer_p256_key.y.len() > 32 {
            bytes.extend_from_slice(&peer_p256_key.y[peer_p256_key.y.len() - 32..]);
        } else {
            bytes.extend_from_slice(&peer_p256_key.y);
        }

        let encoded_point = EncodedPoint::from_bytes(bytes)?;
        let peer_key = PublicKey::from_encoded_point(&encoded_point).unwrap();
        let priv_key = self.state.private_key.as_ref().unwrap();

        let dhs = diffie_hellman(priv_key.to_nonzero_scalar(), peer_key.as_affine());
        let derived_secret = Sha256::digest(dhs.raw_secret_bytes());

        let mut ukey_info: Vec<u8> = vec![];
        ukey_info.extend_from_slice(self.state.client_init_msg_data.as_ref().unwrap());
        ukey_info.extend_from_slice(self.state.server_init_data.as_ref().unwrap());

        let auth_label = "UKEY2 v1 auth".as_bytes();
        let next_label = "UKEY2 v1 next".as_bytes();

        let auth_string = hkdf_extract_expand(auth_label, &derived_secret, &ukey_info, 32)?;
        let next_secret = hkdf_extract_expand(next_label, &derived_secret, &ukey_info, 32)?;

        let salt_hex = "82AA55A0D397F88346CA1CEE8D3909B95F13FA7DEB1D4AB38376B8256DA85510";
        let salt =
            hex::decode(salt_hex).map_err(|e| anyhow!("Failed to decode salt_hex: {}", e))?;

        let d2d_client = hkdf_extract_expand(&salt, &next_secret, "client".as_bytes(), 32)?;
        let d2d_server = hkdf_extract_expand(&salt, &next_secret, "server".as_bytes(), 32)?;

        let key_salt_hex = "BF9D2A53C63616D75DB0A7165B91C1EF73E537F2427405FA23610A4BE657642E";
        let key_salt = hex::decode(key_salt_hex)
            .map_err(|e| anyhow!("Failed to decode key_salt_hex: {}", e))?;

        let client_key = hkdf_extract_expand(&key_salt, &d2d_client, "ENC:2".as_bytes(), 32)?;
        let client_hmac_key = hkdf_extract_expand(&key_salt, &d2d_client, "SIG:1".as_bytes(), 32)?;
        let server_key = hkdf_extract_expand(&key_salt, &d2d_server, "ENC:2".as_bytes(), 32)?;
        let server_hmac_key = hkdf_extract_expand(&key_salt, &d2d_server, "SIG:1".as_bytes(), 32)?;

        self.update_state(
            |e| {
                e.decrypt_key = Some(server_key);
                e.recv_hmac_key = Some(server_hmac_key);
                e.encrypt_key = Some(client_key);
                e.send_hmac_key = Some(client_hmac_key);
                e.pin_code = Some(to_four_digit_string(&auth_string));
                e.encryption_done = true;
            },
            false,
        );

        info!("Pin code: {:?}", self.state.pin_code);

        Ok(())
    }

    async fn send_ukey2_alert(&mut self, atype: AlertType) -> Result<(), anyhow::Error> {
        let alert = Ukey2Alert {
            r#type: Some(atype.into()),
            error_message: None,
        };

        let data = Ukey2Message {
            message_type: Some(atype.into()),
            message_data: Some(alert.encode_to_vec()),
        };

        self.send_frame(data.encode_to_vec()).await
    }

    async fn send_encrypted_frame(
        &mut self,
        frame: &sharing_nearby::Frame,
    ) -> Result<(), anyhow::Error> {
        let frame_data = frame.encode_to_vec();
        let body_size = frame_data.len();

        let payload_header = PayloadHeader {
            id: Some(rand::thread_rng().gen_range(i64::MIN..i64::MAX)),
            r#type: Some(payload_header::PayloadType::Bytes.into()),
            total_size: Some(body_size as i64),
            is_sensitive: Some(false),
            ..Default::default()
        };

        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(0),
                flags: Some(0),
                body: Some(frame_data),
            }),
            payload_header: Some(payload_header.clone()),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        // Send lastChunk
        let transfer = PayloadTransferFrame {
            packet_type: Some(PacketType::Data.into()),
            payload_chunk: Some(PayloadChunk {
                offset: Some(body_size as i64),
                flags: Some(1), // lastChunk
                body: Some(vec![]),
            }),
            payload_header: Some(payload_header),
            ..Default::default()
        };

        let wrapper = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(
                    location_nearby_connections::v1_frame::FrameType::PayloadTransfer.into(),
                ),
                payload_transfer: Some(transfer),
                ..Default::default()
            }),
        };

        // Encrypt and send offline
        self.encrypt_and_send(&wrapper).await?;

        Ok(())
    }

    async fn encrypt_and_send(&mut self, frame: &OfflineFrame) -> Result<(), anyhow::Error> {
        let d2d_msg = DeviceToDeviceMessage {
            sequence_number: Some(self.get_server_seq_inc()),
            message: Some(frame.encode_to_vec()),
        };

        let key = self.state.encrypt_key.as_ref().unwrap();
        let msg_data = d2d_msg.encode_to_vec();
        let iv = gen_random(16);

        let mut cipher = Cipher::new_256(&key[..AES_256_KEY_LEN].try_into().unwrap());
        cipher.set_auto_padding(true);
        let encrypted = cipher.cbc_encrypt(&iv, &msg_data);

        let hb = HeaderAndBody {
            body: encrypted,
            header: Header {
                encryption_scheme: EncScheme::Aes256Cbc.into(),
                signature_scheme: SigScheme::HmacSha256.into(),
                iv: Some(iv),
                public_metadata: Some(
                    GcmMetadata {
                        r#type: Type::DeviceToDeviceMessage.into(),
                        version: Some(1),
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            },
        };

        let mut hmac = HmacSha256::new_from_slice(self.state.send_hmac_key.as_ref().unwrap())?;
        hmac.update(&hb.encode_to_vec());
        let result = hmac.finalize();

        let smsg = SecureMessage {
            header_and_body: hb.encode_to_vec(),
            signature: result.into_bytes().to_vec(),
        };

        self.send_frame(smsg.encode_to_vec()).await?;

        Ok(())
    }

    async fn send_keepalive(&mut self, ack: bool) -> Result<(), anyhow::Error> {
        let ack_frame = location_nearby_connections::OfflineFrame {
            version: Some(location_nearby_connections::offline_frame::Version::V1.into()),
            v1: Some(location_nearby_connections::V1Frame {
                r#type: Some(location_nearby_connections::v1_frame::FrameType::KeepAlive.into()),
                keep_alive: Some(KeepAliveFrame { ack: Some(ack) }),
                ..Default::default()
            }),
        };

        if self.state.encryption_done {
            self.encrypt_and_send(&ack_frame).await
        } else {
            self.send_frame(ack_frame.encode_to_vec()).await
        }
    }

    async fn send_frame(&mut self, data: Vec<u8>) -> Result<(), anyhow::Error> {
        let length = data.len();

        // Prepare length prefix in big-endian format
        let length_bytes = [
            (length >> 24) as u8,
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
        ];

        let mut prefixed_length = Vec::with_capacity(length + 4);
        prefixed_length.extend_from_slice(&length_bytes);
        prefixed_length.extend_from_slice(&data);

        self.socket.write_all(&prefixed_length).await?;
        self.socket.flush().await?;

        Ok(())
    }

    fn get_server_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.server_seq += 1;
            },
            false,
        );

        self.state.server_seq
    }

    fn get_client_seq_inc(&mut self) -> i32 {
        self.update_state(
            |e| {
                e.client_seq += 1;
            },
            false,
        );

        self.state.client_seq
    }

    fn update_state<F>(&mut self, f: F, inform: bool)
    where
        F: FnOnce(&mut InnerState),
    {
        f(&mut self.state);

        if !inform {
            return;
        }

        let _ = self.sender.send(ChannelMessage {
            id: self.state.id.clone(),
            direction: ChannelDirection::LibToFront,
            state: Some(self.state.state.clone()),
            meta: self.state.transfer_metadata.clone(),
            ..Default::default()
        });
    }
}
