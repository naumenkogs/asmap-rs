use crate::common::*;

/// Contains the mapping of each prefix to its bottleneck asn.
#[derive(Debug, PartialEq)]
pub(crate) struct FindBottleneck {
    prefix_asn: HashMap<Address, u32>,
}

impl FindBottleneck {

    fn open_files(dir: &PathBuf) -> Result<Vec<GzDecoder<BufReader<File>>>> {
        let mut file_decoders = Vec::new();
        for entry in fs::read_dir(dir).map_err(|io_error| Error::IoError {
            io_error,
            path: dir.to_path_buf(),
        })? {
            let entry = entry.map_err(|io_error| Error::IoError {
                io_error,
                path: dir.to_path_buf(),
            })?;
            let path = entry.path();
            println!("Acquiring a reader for file `{}`", &path.display());
            let buffer = BufReader::new(File::open(&path).map_err(|io_error| Error::IoError {
                io_error,
                path: path.into(),
            })?);

            let decoder = GzDecoder::new(buffer);
            file_decoders.push(decoder);
        }
        return Ok(file_decoders);
    }

    /// Creates a new `FindBottleneck`, reads and parses mrt files, locates prefix and asn bottleneck
    pub(crate) fn locate(dir_sorted: &PathBuf, dir_unsorted: &PathBuf, out: &mut dyn Write) -> Result<()> {
        // This function uses an optimization to not download everything in RAM.
        // Instead, it would read file contents in batches, where every batch corresponds
        // to a prefix range. For example, it will first process all prefixes from
        // 0.0.0.0 to 15.255.255.255.255.
        
        // This assumes that inputs are sorted by prefix,
        // which is *almost* correct for all RIPE files, and anything produced by Quagga.
        // "Even though the MRT format specification does not seem to require so,
        // current implementations to produce the RIB dump file (such as Quagga)
        // typically store prefixes sorted in the output."
        // Source: https://labs.ripe.net/Members/yasuhiro_ohara/bgpdump2

        // All unsorted files should be stored and processed separately. Since there's a minority
        // of them, we'll first load all them in memory, and then, when processing sorted files,
        // cherry-pick the records from unsorted (those relevant for a given batch).

        let mut file_decoders_sorted = Self::open_files(dir_sorted)?;
        let mut file_decoders_unsorted = Self::open_files(dir_unsorted)?;

        // First collect all the files which does not preserve order of records by prefix.
        // Use them every time we are processing batch.
        let mut mrt_hm_unsorted = HashMap::new();
        for i in 0..file_decoders_unsorted.len() {
            // Load all data at once without batching.
            Self::parse_mrt(&mut file_decoders_unsorted[i], &mut mrt_hm_unsorted, u8::max_value(), &mut HashMap::new())?;
        }

        // Used to keep track of the last element of the batch.
        // Since a prefix may have multiple records in a file,
        // without this workaround it may be either not written at all or written twice.
        let mut next_mrt_hm = HashMap::new();
        let step: u8 = 1 << 4; // should be a power of 2
        for current_start_high_octet in (0..u8::max_value()).step_by(step as usize) {
            let current_end_high_octet: u8 = current_start_high_octet.saturating_add(step);

            let mut mrt_hm = next_mrt_hm.clone();
            next_mrt_hm.clear();

            for i in 0..file_decoders_sorted.len() {
                println!(
                    "Processing chunk up to {}.255.255.255 plus one (both ipv4 and ipv6) in file {}/{}",
                    current_end_high_octet,
                    i,
                    file_decoders_sorted.len()
                );
                Self::parse_mrt(&mut file_decoders_sorted[i], &mut mrt_hm, current_end_high_octet, &mut next_mrt_hm)?;
            }

            // Move the relevant data from unsorted files to current batch records.
            for (prefix, paths_from_sorted) in &mut mrt_hm {
                match mrt_hm_unsorted.get(prefix) {
                    Some(paths_from_unsorted) => {
                        for path in paths_from_unsorted {
                            paths_from_sorted.insert(path.to_vec());
                        }
                        mrt_hm_unsorted.remove(&prefix);
                    }
                    None => continue
                }           
            }

            let mut bottleneck = FindBottleneck {
                prefix_asn: HashMap::new(),
            };
            bottleneck.find_as_bottleneck(&mut mrt_hm)?;
            bottleneck.write_bottleneck(out)?;
        }

        // Write the remaining values from unsorted files.
        let mut bottleneck = FindBottleneck {
            prefix_asn: HashMap::new(),
        };
        bottleneck.find_as_bottleneck(&mut mrt_hm_unsorted)?;
        bottleneck.write_bottleneck(out)?;

        Ok(())
    }

    /// Creates a mapping between a prefix and all of its asn paths, gets the common asns from
    /// those paths, and considers the last asn (the asn farthest from the originating hop) from
    /// the common asns to be the bottleneck.
    fn find_as_bottleneck(
        &mut self,
        mrt_hm: &mut HashMap<Address, HashSet<Vec<u32>>>,
    ) -> Result<(), Error> {
        // In the vector value, the first element is the final AS (so the actual AS of the IP,
        // not some AS on the path). The last element is the critical AS on the path that
        // determines the bottleneck.
        let mut prefix_to_common_suffix: HashMap<Address, Vec<u32>> = HashMap::new();

        Self::find_common_suffix(mrt_hm, &mut prefix_to_common_suffix)?;

        for (addr, mut as_path) in prefix_to_common_suffix {
            let asn = match as_path.pop() {
                Some(a) => a,
                None => panic!("ERROR: No ASN"), // TODO: Handle error
            };
            self.prefix_asn.insert(addr, asn);
        }

        Ok(())
    }

    /// Logic that finds the mapping of each prefix and the asns common to all of the prefix's asn
    /// paths.
    fn find_common_suffix(
        mrt_hm: &mut HashMap<Address, HashSet<Vec<u32>>>,
        prefix_to_common_suffix: &mut HashMap<Address, Vec<u32>>,
    ) -> Result<(), Error> {
        'outer: for (prefix, as_paths) in mrt_hm.iter() {
            let mut as_paths_sorted: Vec<&Vec<u32>> = as_paths.iter().collect();

            as_paths_sorted.sort_by(|a, b| a.len().cmp(&b.len())); // descending

            let mut rev_common_suffix: Vec<u32> = as_paths_sorted[0].to_vec();
            rev_common_suffix.reverse();

            for as_path in as_paths_sorted.iter().skip(1) {
                // first one is already in rev_common_suffix
                let mut rev_as_path: Vec<u32> = as_path.to_vec();
                rev_as_path.reverse();

                // Every IP should always belong to only one AS
                if rev_common_suffix.first() != rev_as_path.first() {
                    warn!(
                            "Every IP should belong to one AS. Prefix: `{:?}` has anomalous AS paths: `{:?}`.",
                            &prefix, &as_paths
                        );
                    continue 'outer;
                }

                // first element is already checked
                for i in 1..rev_common_suffix.len() {
                    if rev_as_path[i] != rev_common_suffix[i] {
                        rev_common_suffix.truncate(i);
                        break;
                    }
                }
            }
            // rev_common_suffix.reverse();
            prefix_to_common_suffix
                .entry(*prefix)
                .or_insert(rev_common_suffix);
        }

        Ok(())
    }

    /// Parses the mrt formatted data, extracting the pertinent `PEER_INDEX_TABLE` values
    /// containing the prefix and associated as paths.
    fn parse_mrt(
        reader: &mut dyn Read,
        mrt_hm: &mut HashMap<Address, HashSet<Vec<u32>>>,
        current_end_high_octet: u8,
        next_mrt_hm: &mut HashMap<Address, HashSet<Vec<u32>>>
    ) -> Result<()> {
        let mut reader = Reader { stream: reader };
        loop {
            match reader.read() {
                Ok(header_record) => match header_record {
                    Some((_, record)) => match record {
                        Record::TABLE_DUMP_V2(tdv2_entry) => match tdv2_entry {
                            TABLE_DUMP_V2::RIB_IPV4_UNICAST(entry) => {
                                let ip = Self::format_ip(&entry.prefix, true)?;
                                let mask = entry.prefix_length;
                                if let IpAddr::V4(ipv4) = ip {
                                    if ipv4.octets()[0] > current_end_high_octet {
                                        Self::match_rib_entry(entry.entries, ip, mask, next_mrt_hm)?;
                                        break;
                                    }
                                }
                                Self::match_rib_entry(entry.entries, ip, mask, mrt_hm)?;
                            }
                            TABLE_DUMP_V2::RIB_IPV6_UNICAST(entry) => {
                                let ip = Self::format_ip(&entry.prefix, false)?;
                                let mask = entry.prefix_length;
                                if let IpAddr::V6(ipv6) = ip {
                                    if ipv6.octets()[0] > current_end_high_octet {
                                        Self::match_rib_entry(entry.entries, ip, mask, next_mrt_hm)?;
                                        break;
                                    }
                                }
                                Self::match_rib_entry(entry.entries, ip, mask, mrt_hm)?;
                            }
                            _ => continue,
                        },
                        _ => continue,
                    },
                    None => break,
                },
                Err(e) => match e.kind() {
                    std::io::ErrorKind::InvalidInput => {
                        println!("Invalid gzip header. Skipping file.")
                    }
                    other_error => println!(
                        "Problem with gzip mrt file. `{:?}`. Skipping file.",
                        other_error
                    ),
                },
            }
        }
        Ok(())
    }

    /// Format IPV4 and IPV6 from slice.
    fn format_ip(ip: &[u8], is_ipv4: bool) -> Result<IpAddr> {
        let pad = &[0; 17];
        let ip = [ip, pad].concat();
        if is_ipv4 {
            Ok(IpAddr::V4(std::net::Ipv4Addr::new(
                ip[0], ip[1], ip[2], ip[3],
            )))
        } else {
            Ok(IpAddr::V6(std::net::Ipv6Addr::new(
                u16::from_be_bytes([ip[0], ip[1]]),
                u16::from_be_bytes([ip[2], ip[3]]),
                u16::from_be_bytes([ip[4], ip[5]]),
                u16::from_be_bytes([ip[7], ip[8]]),
                u16::from_be_bytes([ip[9], ip[10]]),
                u16::from_be_bytes([ip[11], ip[12]]),
                u16::from_be_bytes([ip[13], ip[14]]),
                u16::from_be_bytes([ip[15], ip[16]]),
            )))
        }
    }

    /// Parse each RIB Entry.
    fn match_rib_entry(
        entries: Vec<mrt_rs::records::tabledump::RIBEntry>,
        ip: IpAddr,
        mask: u8,
        mrt_hm: &mut HashMap<Address, HashSet<Vec<u32>>>,
    ) -> Result<()> {
        let addr = Address { ip, mask };

        for rib_entry in entries {
            match AsPathParser::parse(&rib_entry.attributes) {
                Ok(mut as_path) => {
                    as_path.dedup();
                    mrt_hm
                        .entry(addr)
                        .or_insert_with(HashSet::new)
                        .insert(as_path);
                }
                Err(e) => info!("ERROR: {:?}. ", e), // TODO: Handle error
            };
        }
        Ok(())
    }

    /// Writes the asn bottleneck result to a time stamped file in a specified or default location
    pub(crate) fn write(temp_result_file: &File, out: Option<&Path>) -> Result<()> {
        let epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let file_name = format!("bottleneck.{}.txt", epoch.as_secs());
        let dst;
        if let Some(path) = out {
            dst = path.join(file_name);
        } else {
            dst = PathBuf::from(file_name);
        };
        let file = File::create(&dst).map_err(|io_error| Error::IoError {
            io_error,
            path: dst.to_path_buf(),
        })?;

        let mut buf_reader = BufReader::new(temp_result_file);
        let mut buf_writer = BufWriter::new(file);

        io::copy(&mut buf_reader, &mut buf_writer).map_err(|io_error| Error::IoError {
            io_error,
            path: dst.to_path_buf(),
        })?;

        Ok(())
    }

    /// Helper write function
    fn write_bottleneck(self, out: &mut dyn Write) -> Result<(), Error> {
        for (key, value) in self.prefix_asn {
            let text = format!("{}/{}|{:?}", key.ip, key.mask, value);
            writeln!(out, "{}", &text).unwrap();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_mrt_hm() -> Result<HashMap<Address, HashSet<Vec<u32>>>, Error> {
        let mut mrt_hm: HashMap<Address, HashSet<Vec<u32>>> = HashMap::new();
        let ip_str = "1.0.139.0";
        let addr = Address {
            ip: IpAddr::from_str(ip_str).map_err(|addr_parse| Error::AddrParse {
                addr_parse,
                bad_addr: ip_str.to_string(),
            })?,
            mask: 24,
        };

        let mut asn_paths = HashSet::new();
        asn_paths.insert(vec![2497, 38040, 23969]);
        asn_paths.insert(vec![25152, 6939, 4766, 38040, 23969]);
        asn_paths.insert(vec![4777, 6939, 4766, 38040, 23969]);
        mrt_hm.insert(addr, asn_paths);

        let ip_str = "1.0.204.0";
        let addr = Address {
            ip: IpAddr::from_str(ip_str).map_err(|addr_parse| Error::AddrParse {
                addr_parse,
                bad_addr: ip_str.to_string(),
            })?,
            mask: 22,
        };
        let mut asn_paths = HashSet::new();
        asn_paths.insert(vec![2497, 38040, 23969]);
        asn_paths.insert(vec![4777, 6939, 4766, 38040, 23969]);
        asn_paths.insert(vec![25152, 2914, 38040, 23969]);
        mrt_hm.insert(addr, asn_paths);

        let ip_str = "1.0.6.0";
        let addr = Address {
            ip: IpAddr::from_str(ip_str).map_err(|addr_parse| Error::AddrParse {
                addr_parse,
                bad_addr: ip_str.to_string(),
            })?,
            mask: 24,
        };
        let mut asn_paths = HashSet::new();
        asn_paths.insert(vec![2497, 4826, 38803, 56203]);
        asn_paths.insert(vec![25152, 6939, 4826, 38803, 56203]);
        asn_paths.insert(vec![4777, 6939, 4826, 38803, 56203]);
        mrt_hm.insert(addr, asn_paths);

        Ok(mrt_hm)
    }

    #[test]
    fn finds_common_suffix_from_mrt_hashmap() -> Result<(), Error> {
        let mut want: HashMap<Address, Vec<u32>> = HashMap::new();
        want.insert(Address::from_str("1.0.139.0/24")?, vec![23969, 38040]);
        want.insert(Address::from_str("1.0.204.0/22")?, vec![23969, 38040]);
        want.insert(Address::from_str("1.0.6.0/24")?, vec![56203, 38803, 4826]);

        let mut mrt_hm = setup_mrt_hm()?;
        let mut have: HashMap<Address, Vec<u32>> = HashMap::new();

        assert_eq!(
            FindBottleneck::find_common_suffix(&mut mrt_hm, &mut have)?,
            ()
        );
        assert_eq!(have, want);

        Ok(())
    }

    #[test]
    fn finds_as_bottleneck_from_mrt_hashmap() -> Result<(), Error> {
        let mut want = FindBottleneck {
            prefix_asn: HashMap::new(),
        };
        want.prefix_asn
            .insert(Address::from_str("1.0.139.0/24")?, 38040);
        want.prefix_asn
            .insert(Address::from_str("1.0.204.0/22")?, 38040);
        want.prefix_asn
            .insert(Address::from_str("1.0.6.0/24")?, 4826);

        let mut have = FindBottleneck {
            prefix_asn: HashMap::new(),
        };
        let mut mrt_hm = setup_mrt_hm()?;
        have.find_as_bottleneck(&mut mrt_hm)?;

        assert_eq!(have, want);

        Ok(())
    }

    #[test]
    fn ipaddr_from_ipv6_short() -> Result<(), Error> {
        let have = FindBottleneck::format_ip(&[32, 1, 3, 24], false)?;
        assert_eq!("2001:318::".parse(), Ok(have));

        Ok(())
    }

    #[test]
    fn ipaddr_from_ipv6_long() -> Result<(), Error> {
        let have = FindBottleneck::format_ip(&[32, 1, 2, 248, 16, 8], false)?;
        assert_eq!("2001:2f8:1008::".parse(), Ok(have));

        Ok(())
    }
}
