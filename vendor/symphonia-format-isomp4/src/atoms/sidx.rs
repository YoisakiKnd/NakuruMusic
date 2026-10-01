// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{decode_error, Result};
use symphonia_core::io::ReadBytes;

use crate::atoms::{Atom, AtomHeader};

#[derive(Debug)]
pub enum ReferenceType {
    Segment,
    Media,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxReference {
    pub reference_type: ReferenceType,
    pub reference_size: u32,
    pub subsegment_duration: u32,
    // pub starts_with_sap: bool,
    // pub sap_type: u8,
    // pub sap_delta_time: u32,
}

/// Segment index atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxAtom {
    /// Atom header.
    header: AtomHeader,
    pub reference_id: u32,
    pub timescale: u32,
    pub earliest_pts: u64,
    pub first_offset: u64,
    pub references: Vec<SidxReference>,
}

impl SidxAtom {
    /// A complete, single-level index lets the demuxer avoid scanning every
    /// media-data atom before starting a long fragmented MP4 stream.
    pub fn covers_file_end(&self, file_len: u64) -> bool {
        !self.references.is_empty()
            && self.timescale != 0
            && self
                .references
                .iter()
                .all(|reference| matches!(reference.reference_type, ReferenceType::Media))
            && self
                .references
                .iter()
                .try_fold(self.first_offset, |offset, reference| {
                    offset.checked_add(u64::from(reference.reference_size))
                })
                == Some(file_len)
    }

    /// Locate the media fragment containing a timestamp in index timebase units.
    pub fn fragment_at(&self, timestamp: u64) -> Option<(u64, u64)> {
        let mut offset = self.first_offset;
        let mut start = self.earliest_pts;
        for reference in &self.references {
            let end = start.checked_add(u64::from(reference.subsegment_duration))?;
            if timestamp < end {
                return Some((offset, start));
            }
            offset = offset.checked_add(u64::from(reference.reference_size))?;
            start = end;
        }
        None
    }
}

impl Atom for SidxAtom {
    fn header(&self) -> AtomHeader {
        self.header
    }

    fn read<B: ReadBytes>(reader: &mut B, header: AtomHeader) -> Result<Self> {
        // The anchor point for segment offsets is the first byte after this atom.
        let anchor = reader.pos() + header.data_len;

        let (version, _) = AtomHeader::read_extra(reader)?;

        let reference_id = reader.read_be_u32()?;
        let timescale = reader.read_be_u32()?;

        let (earliest_pts, first_offset) = match version {
            0 => (
                u64::from(reader.read_be_u32()?),
                anchor + u64::from(reader.read_be_u32()?),
            ),
            1 => (reader.read_be_u64()?, anchor + reader.read_be_u64()?),
            _ => {
                return decode_error("isomp4: invalid sidx version");
            }
        };

        let _reserved = reader.read_be_u16()?;
        let reference_count = reader.read_be_u16()?;

        let mut references = Vec::new();

        for _ in 0..reference_count {
            let reference = reader.read_be_u32()?;
            let subsegment_duration = reader.read_be_u32()?;

            let reference_type = match (reference & 0x8000_0000) != 0 {
                false => ReferenceType::Media,
                true => ReferenceType::Segment,
            };

            let reference_size = reference & !0x8000_0000;

            // Ignore SAP
            let _ = reader.read_be_u32()?;

            references.push(SidxReference {
                reference_type,
                reference_size,
                subsegment_duration,
            });
        }

        Ok(SidxAtom {
            header,
            reference_id,
            timescale,
            earliest_pts,
            first_offset,
            references,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atoms::AtomType;

    #[test]
    fn complete_index_locates_fragment_boundaries() {
        let index = SidxAtom {
            header: AtomHeader {
                atype: AtomType::SegmentIndex,
                atom_len: 0,
                data_len: 0,
            },
            reference_id: 1,
            timescale: 1000,
            earliest_pts: 250,
            first_offset: 100,
            references: vec![
                SidxReference {
                    reference_type: ReferenceType::Media,
                    reference_size: 20,
                    subsegment_duration: 1000,
                },
                SidxReference {
                    reference_type: ReferenceType::Media,
                    reference_size: 30,
                    subsegment_duration: 1000,
                },
            ],
        };
        assert!(index.covers_file_end(150));
        assert!(!index.covers_file_end(151));
        assert_eq!(index.fragment_at(250), Some((100, 250)));
        assert_eq!(index.fragment_at(1249), Some((100, 250)));
        assert_eq!(index.fragment_at(1250), Some((120, 1250)));
        assert_eq!(index.fragment_at(2250), None);
    }
}
