# Validator selection for the measurements (mainnet, after Alpenglow)

Pre-study from 2026-10-28. The run's selection is made by `plan` with the rules in [PREREGISTRATION.md](../PREREGISTRATION.md); stake and versions here are from the survey day.

Read-only survey on 2026-10-28: public RPC `https://api.mainnet-beta.solana.com`, Jito's public Kobe API, and open-source code and docs (links in section 8).

## 1. Snapshot

| Parameter | Value |
|---|---|
| Snapshot time (UTC) | 2026-10-28 10:04:20 (`getEpochInfo`; the other calls within ~3 minutes) |
| Epoch | 1070 (start: slot 462,240,000, blockTime 2026-10-27 22:29:55 UTC) |
| Slot | 462,435,726 (slotIndex 195,726 of 432,000) |
| `getBlockProduction` | current epoch up to slot 462,435,781 (~195.8k slots, ~45% of the epoch) |
| RPC node version | solana-core 4.3.2, feature-set 3383571666 |
| Total active stake | 437,290,026 SOL, 691 vote accounts; 0.05% threshold ≈ 218,645 SOL |
| Average slot time | ~213 ms (over the epoch: 41,848 s / 196,552 slots; `getRecentPerformanceSamples` gives 277–281 slots per 60 s) |
| Leader window length | 4 consecutive slots (~0.85 s) |
| End of epoch 1070 (estimate) | ~2026-10-29 00:00 UTC (~235k slots left × 0.213 s) |

The 4-slot window comes from `getLeaderSchedule` for epoch 1070. Its 432,000 slots split into runs of a single leader: 105,767 runs of 4 slots, 1,082 of 8 and 23 of 12. Every run starts at a slot divisible by 4. Runs of 8 and 12 slots appear when the same leader gets adjacent windows, so the base window after Alpenglow is still 4 slots.

Signs that Alpenglow is active:
- in `getRecentPerformanceSamples`, `numTransactions == numNonVoteTransactions`, so blocks carry no vote transactions (in Votor, votes travel outside blocks)
- according to public sources, activation happened in epoch 1064, slot 459,653,000, on 2026-10-21

All of epoch 1070 falls after the upgrade.

The raw RPC responses of this survey were not kept; `plan` saves its own snapshot under `data/snapshots/` for the actual run.

`getBlockProduction` for the range of the previous epoch 1069 failed: the public RPC rejected it twice with `403 "Your IP or provider is blocked from this endpoint"`. The skip rate is therefore computed over the current epoch.

**Important for the collector:** blocks already contain **version 1** transactions, 37–53% of the transactions in the 6 sample blocks. `getBlock` with `maxSupportedTransactionVersion: 0` returns error `-32015`; it needs `maxSupportedTransactionVersion: 1`.

## 2. How we identify the client

### 2.1 What `getClusterNodes` provides

Response fields: `clientId`, `featureSet`, `gossip`, `pubkey`, `pubsub`, `rpc`, `serveRepair`, `shredVersion`, `tpu`, `tpuForwards`, `tpuForwardsQuic`, `tpuQuic`, `tpuVote`, `tvu`, `version`.

- `version` cannot tell the forks apart: Agave, Jito-Solana, BAM, Harmonic-Agave, Rakurai and Raiku all report the same string `4.3.2`.
- `featureSet` cannot either: all of them share 3383571666. It only separates the Firedancer line (1766517360).
- The main identifier is `clientId`, a u16 from gossip ContactInfo that the client binary sets itself. Value table in Agave (`version/src/client_ids.rs`):

| u16 | `clientId` | What it is |
|---|---|---|
| 0 | SolanaLabs | old Solana Labs client |
| 1 | JitoLabs | Jito-Solana (`jito-solana` sets `this_client() = JitoLabs`) |
| 2 | Frankendancer | Firedancer + Agave hybrid, versions like `0.1xxx.4yyzz` (e.g. v0.1204.40300) |
| 3 | Agave | vanilla Agave (Anza) |
| 4 | AgavePaladin | Paladin |
| 5 | Firedancer | full Firedancer, versions like `YY.MM.patch` (releases "Firedancer Mainnet Release v26.10.4/5") |
| 6 | AgaveBam | Jito-Solana connected to a BAM node (see below) |
| 7 | Sig | Syndica Sig |
| 8 | Rakurai | Jito-Solana fork with a proprietary scheduler |
| 9 | HarmonicFiredancer | Harmonic "Samba" (Firedancer) |
| 10 | HarmonicAgave | Harmonic "Salsa" (Agave; the `harmonic/salsa` repo sets `this_client() = HarmonicAgave`) |
| 11 | HarmonicFrankendancer | Harmonic on Frankendancer |
| 12 | FireBAM | Firedancer + BAM |
| 13 | Raiku | Raiku |
| other | Unknown(n) | unknown id |

### 2.2 Ambiguous cases

Jito-Solana and Agave look the same in `version` and `featureSet`; only `clientId` (JitoLabs or Agave) separates them. There are two cross-checks:
1. Jito's Kobe API (`kobe.mainnet.jito.network/api/v1/validators`, epoch 1070):
   - 147 of 152 JitoLabs have `running_jito=true`; the other 5 (stake ≈0) have no Kobe record
   - 19 of 20 Agave have no Kobe record (the exception is `anza1…` (Anza), see anomalies)
2. Block behavior (one sample per leader, so the evidence is weak). Transactions that touch Jito's 8 tip accounts:

| Leader | Client | tip transactions in the block |
|---|---|---|
| HEL1 | JitoLabs | 16 |
| DRpb | AgaveBam | 13 |
| 9eGr | Firedancer | 14 |
| Fd7b | HarmonicAgave | 4 |
| 7QQG | Agave | 2 (both signed by a third party, `AAuz…`, not the leader's crank) |
| 5Us1 | Agave | 0 |

For BAM (AgaveBam), `jito-solana/core/src/bam_manager.rs` shows that:
- `clientId` changes at runtime: AgaveBam while the node is connected to BAM, JitoLabs after it disconnects
- the TPU/TPU-forward addresses in gossip are taken from the BAM node's config (`update_tpu_config`)

In the snapshot, all 356 AgaveBam have `tpuQuic` on port 5010 at an IP that is not the validator's, that is, at the BAM node. According to Helius, the BAM node runs periodic auctions in a TEE, and the validator executes the sequence it receives in FIFO order. So for this group we measure the BAM node's scheduler, not the validator's. Kobe confirms the status: 354 of 356 AgaveBam have `running_bam=true` and `bam_connection_rate ≥ 0.99`.

According to Jito's changelog of 9 October, BAM nodes went into maintenance for the Alpenglow transition (validators fell back to Jito-Agave + Block Engine). Kobe shows that BAM is running again: 362 validators with rate ≥ 0.99 in epoch 1069, 373 in epoch 1070.

> **Consequence:** BAM membership holds only "at the time of the window". Before each window, re-read the leader's `clientId` and `tpuQuic`.

FireBAM (Firedancer + BAM, id 12) works the same way: `tpuQuic` on port 5010 of a BAM node, `running_bam=true` in Kobe. Jito advised against FireBAM during the transition, but in the snapshot 20 nodes run it on 26.10.4/26.10.5.

Harmonic (HarmonicAgave / HarmonicFiredancer), according to its docs:
- the validator sets a Remote TPU as its proxy TPU
- the block is built by the attached builder with the strategy the operator picks: `fba` by default (50 ms buffer, sorted by descending priority fee and tips), `FIFO`, `MREV` (proprietary) or a custom one
- if the builder is unavailable, the validator builds the block with its local scheduler

In the snapshot, 83 of 84 Harmonic validators have `tpuQuic` on shared proxies (`198.13.140.105`, `198.13.140.111`, `64.130.46.242`, `64.130.45.196`, `63.254.166.232`), with a unique port in 122xx–125xx per validator.

> **The strategy of a given validator is not visible from outside.** Rules may differ within the group, and the analysis has to account for that.

Rakurai (id 8) is a Jito-Solana fork. Its scheduler is a proprietary library loaded at runtime; with the library disabled, it behaves like Jito-Agave (per Figment and RPC Fast). Whether the library is active is not visible from outside. In Kobe, Rakurai has `running_jito=true`.

Raiku (id 13) has 1 validator on the network (Raiku itself).

Paladin has id 4 (AgavePaladin) in the enum, but 0 nodes on mainnet. The latest `paladin-solana` release is v3.0.12-paladin (December 2025), i.e. the 3.x branch. Alpenglow needs 4.x, so the group is currently absent.

Frankendancer, HarmonicFrankendancer and Sig have no staked nodes. Gossip has 2 unstaked Frankendancer nodes on `0.101.40300`.

### 2.3 Confidence

| Group | Confidence | Why |
|---|---|---|
| Agave and JitoLabs | high | `clientId` is compiled in, agrees with Kobe and, indirectly, with tip transactions |
| AgaveBam / FireBAM | high, but only at snapshot time | `clientId` and TPU switch dynamically; Kobe agrees in 354 of 356 cases |
| Firedancer | high | separate `clientId`, version format and `featureSet` |
| HarmonicAgave / HarmonicFiredancer | high for the client, **low for the rule** | the builder strategy is not published |
| Rakurai | medium | the client is known, but whether the proprietary scheduler is on is not visible |
| Unknown(n) | unknown | — |

**Caveat:** `clientId` is reported by the node itself, and an operator can fake it. That is why we cross-check it against Kobe and block behavior.

validators.app renders its pages client-side and its API needs a token, so it was not used. It presumably shows the same gossip `clientId`, but this is **not verified**.

### 2.4 Anomalies in the data

- 6 gossip nodes report `clientId=Agave` and version `9.9.99`, but the Firedancer feature-set (1766517360). None has stake. A clear example that `clientId` and `version` are self-reported.
- `anza1…` (Anza) has `clientId=Agave` (4.4.0-rc.0), but Kobe shows `running_jito=true` for it. Kobe may set the flag based on the presence of a tip distribution account.
- 1 AgaveBam (0.31% of stake) is listed in Kobe with `running_bam=false`. It probably switched within the epoch.
- 12 JitoLabs validators publish `tpuQuic` at an IP that is not their own. Another 38 JitoLabs have `tpuQuic` on port 11228, the default port of jito-relayer, which defaults to `packet_delay_ms=50`. Both signals are heuristic.

## 3. Network distribution by client

Counted by vote accounts from `getVoteAccounts`, with `clientId` taken from `getClusterNodes` by identity.

| clientId | vote accounts | not delinquent | stake share, % | passed filters |
|---|---|---|---|---|
| AgaveBam | 356 | 356 | 35.00 | 113 |
| JitoLabs | 152 | 143 | 26.93 | 59 |
| HarmonicAgave | 46 | 46 | 19.90 | 35 |
| HarmonicFiredancer | 38 | 38 | 8.61 | 24 |
| Firedancer | 27 | 27 | 5.17 | 14 |
| FireBAM | 20 | 20 | 1.91 | 7 |
| Rakurai | 10 | 8 | 1.17 | 4 |
| Agave (vanilla) | 20 | 20 | 1.10 | 5 |
| Raiku | 1 | 1 | 0.08 | 1 |
| Unknown(11331) | 1 | 1 | 0.08 | 1 |
| not in gossip | 19 | 1 | 0.05 | 0 |
| Unknown(16) | 1 | 1 | 0.00 | 0 |

By codebase:
- Agave-based: ≈84.2% of stake
- Firedancer-based (Firedancer, HarmonicFiredancer, FireBAM): ≈15.7%

By where transaction order is decided (the main takeaway for the design):
- external BAM sequencer (AgaveBam + FireBAM): ≈36.9%
- external Harmonic builder: ≈28.5%
- the validator's own scheduler (JitoLabs, Firedancer, Rakurai, Agave, Raiku): ≈34.5%

Vanilla Agave holds only 1.1% of stake, but that is enough for a positive control.

Gossip has 3,925 nodes in total. Most of the 2,287 nodes with `clientId=Agave` are unstaked RPC nodes.

## 4. Filters and selection rule

Filters:
1. not delinquent
2. stake ≥ 0.05% of the total (≥ 218,645 SOL)
3. skip rate ≤ 5%, where skip = 1 − blocksProduced/leaderSlots over epoch 1070 up to slot 462,435,781
4. `tpuQuic` present in gossip
5. ≥ 20 leader slots left in epoch 1070 (≥ 5 windows)

Selection rule: in each group, take the top 3 by stake among the validators that pass the filters.

Notes on the filters:
- After Alpenglow, the skip rate is 0–0.9% for almost everyone, so filter 3 removes almost nothing. The nodes that drop out are mostly small ones.
- The "slots in 1071" column is the next epoch's schedule, which is already known (`getLeaderSchedule` at slot 462,672,000).
- Stake and schedule change every epoch, so the selection has to be rerun at every epoch boundary.

Marks in the TPU QUIC column:
- "IP≠gossip": the IP in `tpuQuic` differs from the IP in `gossip`
- "BAM node": port 5010
- "Harmonic Remote TPU": shared Harmonic proxy
- "jito-relayer port": port 11228

## 5. Selected validators

Recommendation for the main run: 5 groups × 3 = 15 validators. (The final plan instead takes the top 3 of every client group: 23 validators in 8 groups on 2026-11-03.)
- Agave (positive control)
- JitoLabs
- AgaveBam
- Firedancer
- HarmonicAgave

That is 5 different clients against the requirement of "≥ 6 validators, ≥ 3 clients". Together they cover all three ordering mechanisms and ~88% of stake.

Reserve or extension:
- Rakurai, the only alternative scheduler inside the validator itself, with a direct TPU
- HarmonicFiredancer
- FireBAM

The tables below are in descending order of group stake share.

#### Agave (vanilla, positive control)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `7QQGNm3ptwinipDCyaCF7jY5katgmFUu1ieP2f7nwLpE` | `8BMMDVM8Tb88a2Hx1VSZ1TKmH1rMHbVU7xVafMk9JrEW` | — | 4.3.2 | 0.429 | 0.33% (913/916) | `5.199.172.176:12002` | 984 | 1776 |
| `5Us18hLZPXJTS4QVuGSsUw137Dyd2tgBaem24Xsf5nBS` | `AzbQjKoLepbDS8C6hgkRY7UCRwbwZZ9HqdtnWyef9WjX` | — | 4.3.2 | 0.276 | 0.56% (533/536) | `185.189.44.172:12002` | 736 | 1304 |
| `mds4GEuiSgQRqveGyktWpETBFCb4AS2wDnhqwLHcT6Z` | `GLCrZmxWYcrGMA5uRa8mmNnQurGBL2p9zrKk9DKugSZX` | — | 4.4.0-rc.1 | 0.078 | 0.00% (152/152) | `189.1.164.35:8003` | 176 | 372 |

Notes on the Agave group:
- 7QQG and 5Us1 have no on-chain name, and Stakewiz does not know their names either. Their IPs sit next to nodes of one and the same operator: `5.199.172.175–179` and `185.189.44.169–172`, gossip port 12000; the same subnets host 4 AgaveBam, 2 JitoLabs and 1 HarmonicFiredancer. It looks like one operator deliberately runs different clients. That is convenient for the comparison, but the owner should be confirmed.
- The third candidate, `mds4…`, runs **4.4.0-rc.1**, not 4.3.2. If the control needs the same version, the next candidate by the rule is `rsbp8zMHbGCpLoRktmsjspYv77VcjxAzH1KxPCD9BiU` ("Albert is Great", 4.3.2, 0.066%, skip 0% (184/184), `209.250.251.15:8002`, 116 / 244 slots).

#### AgaveBam (Jito-Solana + BAM; the BAM node decides the order)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy` | `3N7s9zXMZ4QqvHQR15t5GNHyqc89KduzMP7423eWiD5g` | binance staking | 4.3.2 | 2.925 | 0.05% (5701/5704) | `208.91.105.53:5010` (IP≠gossip, BAM node) | 6820 | 12768 |
| `5Cchr1XGEg7dbBXByV5NY2ad8jfxAM7HA3x8D56rq9Ux` | `GHViLgbrJdZDPb6sphRbeuPNM9cmjsFuGWzrTF1sKF5n` | Upbit Staking | 4.3.2 | 1.306 | 0.16% (2468/2472) | `63.254.180.11:5010` (IP≠gossip, BAM node) | 3108 | 5716 |
| `Fc6NNdS2j3EmrWbU6Uqt6wsKB5ef72NjaWfNxKYbULGD` | `2tucttroqFNXsrYeMBQ8LfzKNfgwT2rHBzAF6RzbbHEp` | OKX Earn | 4.3.2 | 1.018 | 0.00% (2028/2028) | `198.13.131.139:5010` (IP≠gossip, BAM node) | 2380 | 4396 |

#### JitoLabs (Jito-Solana without BAM)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `HEL1USMZKAL2odpNBj2oCjffnFGaYwmbGmyewGv1e2TU` | `he1iusunGwqrNtafDtLdhsUQDFvo13z9sUa36PauBtk` | Helius | 4.3.2 | 3.810 | 0.00% (7352/7352) | `64.130.57.215:8002` | 9416 | 16544 |
| `E1r4Psq84tHfQ6aPTvvDka4U3u8zPVD7gEUrH25RdxHL` | `8GbwASqdpw4dVcwbWUxbHXMrjyQx2aKkoBR5H1GJF8iD` | Bitwise Onchain Solutions | 4.3.2 | 2.473 | 0.00% (4848/4848) | `64.130.32.153:8002` | 6200 | 10516 |
| `C8Bey3LKVJHVqN6xPTeW8WJfUgFQAeGNBpT4Rp99JP1k` | `26pV97Ce83ZQ6Kz9XT4td8tdoUFPTng8Fb8gPyc53dJx` | Ledger by Figment | 4.3.2 | 2.106 | 0.00% (4188/4188) | `185.26.10.33:8022` | 5060 | 8944 |

All three accept traffic on their own IP; there is no sign of a relayer.

#### HarmonicAgave (the Harmonic builder decides the order; strategy unknown)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `Fd7btgySsrjuo25CJCj7oE7VPMyezDhnx7pZkj2v69Nk` | `CcaHc2L43ZWjwCHART3oZoJvHLAe9hzT2DJNUpBzoTN1` | Figment | 4.3.2 | 3.889 | 0.00% (7808/7808) | `198.13.140.105:12341` (IP≠gossip, Harmonic Remote TPU) | 9256 | 17236 |
| `CAo1dCGYrB6NhHh5xb1cGjUiu86iyCfMTENxgHumSve4` | `51JBzSTU5rAM8gLAVQKgp4WoZerQcSqWC7BitBzgUNAm` | Kraken 2 | 4.3.2 | 2.164 | 0.00% (4350/4350) | `64.130.46.242:12291` (IP≠gossip, Harmonic Remote TPU) | 5244 | 9200 |
| `EkvdKhULbMFqjKBKotAzGi3kwMvMpYNDKJXXQQmi6C1f` | `3ZYJxzCeweSoh2Jj7oCgencFs9y27iKmXJeqYapje1cj` | — | 4.3.2 | 0.942 | 0.17% (1749/1752) | `63.254.166.232:12324` (IP≠gossip, Harmonic Remote TPU) | 2188 | 4052 |

#### HarmonicFiredancer (reserve)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `JUPiTERrZqgf1jUyR7dSkhMx4Kn2qJyekWsg3LT1h4b` | `CatzoSMUkTRidT5DwBxAC2pEtnwMBTpkCepHkFgZDiqb` | Jupiter | 26.10.4 | 2.222 | 0.00% (4588/4588) | `198.13.140.105:12349` (IP≠gossip, Harmonic Remote TPU) | 5313 | 9636 |
| `Awes4Tr6TX8JDzEhCZY2QVNimT6iD1zWHzf1vNyGvpLM` | `DumiCKHVqoCQKD8roLApzR5Fit8qGV5fVQsJV9sTZk4a` | Staking Facilities / MEV | 26.10.4 | 1.600 | 0.00% (3208/3208) | `198.13.140.105:12351` (IP≠gossip, Harmonic Remote TPU) | 3912 | 6908 |
| `87CcUxpyR72ZyfaZirj86mqkbk5uuU2EfnwTegVMLJL5` | `8Pep3GmYiijRALqrMKpez92cxvF4YPTzoZg83uXh14pW` | — | 26.10.4 | 0.875 | 0.00% (1784/1784) | `64.130.45.196:12399` (IP≠gossip, Harmonic Remote TPU) | 2176 | 3760 |

#### Firedancer (full Firedancer, its own pack scheduler)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `9eGrDohdNTAo61DRHyfMuqKWXqYnA3i254Wiszxe8FoY` | `CvSb7wdQAFpHuSpTYTJnX5SYH4hCfQ9VuGnqrKaKwycB` | Galaxy | 26.10.4 | 1.610 | 0.00% (3268/3268) | `86.54.152.249:9007` | 3652 | 7276 |
| `JD549HsbJHeEKKUrKgg4Fj2iyv2RGjsV7NTZjZUrHybB` | `3JD3jMmnR6g88qff2WZ3cMHJRjJMUk9yVZtmYTYeFrXf` | Forward Industries | 26.10.4 | 1.384 | 0.00% (2656/2656) | `86.54.153.246:9007` | 3228 | 6032 |
| `DNVZMSqeRH18Xa4MCTrb1MndNf3Npg4MEwqswo23eWkf` | `9Diao4uo6NpeMud7t5wvGnJ3WxDM7iaYxkGtJM36T4dy` | Firedancer Development | 26.10.5 | 0.439 | 0.00% (940/940) | `23.109.46.35:9007` | 1084 | 1888 |

All Firedancer nodes have `running_jito=true` in Kobe, so they accept bundles through the Block Engine. The selected ones run different versions (26.10.4 and 26.10.5); record the version in every measurement.

#### FireBAM (reserve)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `9jxgosAfHgHzwnxsHw4RAZYaLVokMbnYtmiZBreynGFP` | `HZKopZYvv8v6un2H6KUNVQCnK5zM9emKKezvqhTBSpEc` | — | 26.10.5 | 1.024 | 0.00% (1932/1932) | `198.13.140.227:5010` (IP≠gossip, BAM node) | 2436 | 4568 |
| `LA1NEzryoih6CQW3gwQqJQffK2mKgnXcjSQZSRpM3wc` | `GE6atKoWiQ2pt3zL7N13pjNHjdLVys8LinG8qeJLcAiL` | Laine by SOL Strategies | 26.10.5 | 0.096 | 0.00% (236/236) | `63.254.167.197:5010` (IP≠gossip, BAM node) | 192 | 356 |
| `A4hyMd3FyvUJSRafDUSwtLLaQcxRP4r1BRC9w2AJ1to2` | `J1to2NAwajc8hD6E6kujdQiPn1Bbt2mGKKZLY9kSQKdB` | Jito2 (Experimental) | 26.10.5 | 0.081 | 1.97% (149/152) | `63.254.167.197:5010` (IP≠gossip, BAM node) | 208 | 344 |

#### Rakurai (reserve, candidate for a 6th group)
| identity | vote | name (validator-info) | version | stake % | skip (epoch 1070) | TPU QUIC | slots left in 1070 | slots in 1071 |
|---|---|---|---|---|---|---|---|---|
| `ana2y2YvQ3ZPMwm6qhnN3nJoUSiT3qx5Pvetkq9xcfY` | `4AUED4uj6nSTuANzaAUnGBPJQRmhpDYDwoWJNkoUUBBW` | Anagram Staking | 4.3.2 | 0.518 | 0.00% (992/992) | `45.32.145.112:8002` | 1336 | 2360 |
| `E3mDbFMr8yZcCAZx5wPUFCRmLX31u8Nnj4jPoPc54h3F` | `DierScgiTrz5AM7mddeJLHYNvafym3XhjjdM51AnKevU` | MAVAN | 4.3.2 | 0.203 | 0.74% (405/408) | `51.89.11.197:8002` | 448 | 840 |
| `7PdKhpKz7T39vZHFL1UfcYNDsLvay6hp4KPQq1aUckFf` | `7opSZGmevWhRDyLt5Wu38FZFjUyredGmMki4DNmxDnjd` | Alpha Pro / High APY | 4.3.2 | 0.176 | 0.88% (337/340) | `64.130.41.180:8002` | 388 | 772 |

There are enough windows. The plan is 20 windows per validator, and even the smallest selected validators have plenty to spare (mds4: 176 slots = 44 windows in 1070 and 93 windows in 1071; LA1N: 48 windows in 1070). The bottleneck is not the number of windows but whether we hit the window in time (~0.85 s).

## 6. Incomplete groups and near-misses

| Group | Situation |
|---|---|
| Raiku | 1 validator on the network: `9xcXe4WRAfbVoVgQmFD5xgFHioL9dD7c74pQZ7PeczT` (vote `AVD61fbwxDGuLGCanPWQPxRjdqYk3icssz5u7JH8kbta`, "Raiku", 4.3.2, 0.080%, skip 1.74% (169/172), `70.40.184.198:8002`, 180 / 320 slots). It passes the filters, but that is fewer than 2 validators, so the group cannot be filled. It can be added as a single case. |
| Paladin (AgavePaladin) | 0 nodes. The latest paladin-solana release is v3.0.12 (December 2025); there is none for 4.x/Alpenglow. |
| Frankendancer / HarmonicFrankendancer / Sig | No staked nodes. |
| Unknown(11331) | `Diman2GphWLwECE3swjrAEAJniezpYLxK1edUydiDZau` ("diman", 4.3.2, 0.078%) passes the filters, but its client is unknown. Not used. |
| Agave (vanilla) | 5 of 20 passed the filters; the 3rd pick is marginal (see above). Below the threshold: Stake Shark `9hQqNe3D…` (0.027%, skip 10.7% (25/28)). A series of small nodes `sce*`, `scb*`, `scs*`, `mds1–3` (~0.013% each) runs alpha/beta/rc versions of 4.4–4.5 and falls short on stake; some of them have 8–12% skip. |
| FireBAM | 7 passed the filters, but only one is large (`9jxg…`, 1.02%); the rest are ≤ 0.1%. 3 more nodes at 0.05% were cut by the threshold: `ECZx…`, `idCE…`, `11AMA…`. |
| Rakurai | 4 passed the filters. `2dfgsiSa…` (0.07%) is delinquent and has no slots. `Ed9WjPnZ…`, `3B2mGaZo…`, `SLAY6uN1…` are below 0.05%. |

## 7. Open questions and later checks

1. QUIC reachability. Will the leader's TPU (its own, a BAM node on :5010, Harmonic Remote TPU on :12xxx, Firedancer on :9007) accept an unstaked QUIC connection from our host, how many streams will SWQoS allow, and do the BAM/Harmonic proxies drop outside connections? We check this in our own dry-run (handshake without sending transactions).
2. BAM state changes over time. `clientId` (AgaveBam ↔ JitoLabs) and `tpuQuic` switch when the node connects to or disconnects from a BAM node. Before each window we call `getClusterNodes` for the leader and record `clientId`, `version` and `tpuQuic` in the raw data. The analysis classifies by the state at the window, not by this table.
3. The Harmonic strategy is unknown. Different validators can run `fba`, FIFO, MREV or a custom strategy, and when the builder fails the block is built locally. Expect different rules within the group. Analyze per validator; do not average the group.
4. Rakurai. It is unknown whether the proprietary library is enabled or the node runs in the Jito-Agave fallback mode.
5. Agave and Jito scheduler settings (block production method, pacing, etc.) are not visible from outside. Even "vanilla" Agave may run a non-default config. Check the exact default rule for 4.3.2 in the source.
6. Relayer on JitoLabs. It can only be detected heuristically (port 11228 and/or IP≠gossip). jito-relayer delays packets by 50 ms by default. The selected top 3 JitoLabs show no sign of a relayer.
7. Skip rate of the previous epoch. The public RPC did not return it (403). If needed, get it from another provider. After Alpenglow, skip is ~0 for almost everyone, so it does not matter for the selection.
8. Timing. Slot ~213 ms, window ~0.85 s, half of what it was before the upgrade. Sending has to be tied to the current slot with a margin. Check in the dry-run what share of batches land in the intended window.
9. v1 transactions in blocks: the collector must call `getBlock` with `maxSupportedTransactionVersion: 1`.
10. `clientId` is self-reported. Document the method in the README: `clientId` from `getClusterNodes` plus a cross-check with the Kobe API at measurement time. Ideally also save the validators.app pages by hand (not checked here).
11. Epoch change. Epoch 1070 ends ~2026-10-29 00:00 UTC, and then stake and schedule change. The selection has to be rerun by a script for every epoch (`plan` does this, and `run` also reads the next epoch's schedule).
12. The owner of 7QQG and 5Us1 (the core of the control group) is unknown. Worth checking whether these nodes run a non-standard build behind `clientId=Agave`. Check it via blocks: no tip transactions and no bundles over a large sample.

## 8. Sources

- Agave, `ClientId` table: https://github.com/anza-xyz/agave/blob/master/version/src/client_ids.rs
- Jito-Solana, `this_client() = JitoLabs`: https://github.com/jito-foundation/jito-solana/blob/master/version/src/client_ids.rs
- Jito-Solana, AgaveBam/JitoLabs switching and TPU from the BAM config (commit cf3c278): https://github.com/jito-foundation/jito-solana/blob/master/core/src/bam_manager.rs
- Harmonic Salsa, `this_client() = HarmonicAgave`: https://github.com/harmonic/salsa/blob/master-harmonic/version/src/client_ids.rs
- Harmonic docs:
  - https://docs.harmonic.gg
  - https://docs.harmonic.gg/concepts/harmonic-architecture.md
  - https://docs.harmonic.gg/concepts/scheduling-strategies.md
  - https://docs.harmonic.gg/concepts/streaming-mode.md
- Jito Kobe API: https://kobe.mainnet.jito.network/api/v1/validators (GET and POST `{"epoch":N}`)
- Helius, how BAM works (TPU → BAM node, auctions, FIFO execution, fallback): https://www.helius.dev/blog/block-assembly-marketplace-bam
- Solana Compass, BAM's Alpenglow plan (maintenance mode, FireBAM): https://solanacompass.com/news/jito-details-bams-alpenglow-plan-maintenance-mode-for-all-mainnet-nodes-no-firebam-during-the-transition
- Firedancer releases (v26.10.x for Firedancer mainnet, v0.1204.40300 for Frankendancer): https://github.com/firedancer-io/firedancer/releases
- jito-relayer (port 11228, `packet_delay_ms=50`): https://github.com/jito-foundation/jito-relayer/blob/master/transaction-relayer/src/main.rs
- Rakurai:
  - https://www.figment.io/insights/figment-upgrades-solana-infrastructure-with-rakurai-client/
  - https://rpcfast.com/blog/solana-validator-client-comparison-jito-firedancer-harmonic-rakurai
- Paladin releases: https://github.com/paladin-bladesmith/paladin-solana/releases
- Raiku: https://solanacompass.com/projects/raiku
- Alpenglow activation (epoch 1064, 2026-10-21):
  - https://solanacompass.com/news/anza-sets-alpenglow-mainnet-date-solanas-consensus-switch-is-scheduled-for-october-21-at-about-1450-utc
  - https://incrypted.com/en/solana-rolled-out-its-biggest-upgrade-alpenglow/
