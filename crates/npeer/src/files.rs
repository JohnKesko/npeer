//! Picking files by number: `--only 1,3,5-8` and `--skip 17`.
//!
//! Numbers start at 1, exactly as `npeer info` lists the files.

use std::ops::RangeInclusive;
use std::str::FromStr;

use anyhow::{Result, bail};
use npeer_core::{FileSelection, Metainfo};

/// File numbers as typed on the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileList {
    All,
    Numbers(Vec<RangeInclusive<u32>>),
}

impl FromStr for FileList {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, String> {
        if input.trim().eq_ignore_ascii_case("all") {
            return Ok(FileList::All);
        }
        let mut ranges = Vec::new();
        for part in input.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (start, end) = match part.split_once('-') {
                Some((start, end)) => (file_number(start)?, file_number(end)?),
                None => {
                    let number = file_number(part)?;
                    (number, number)
                }
            };
            if start > end {
                return Err(format!("`{part}` counts backwards; write {end}-{start}"));
            }
            ranges.push(start..=end);
        }
        if ranges.is_empty() {
            return Err("no file numbers given (example: 1,3,5-8)".into());
        }
        Ok(FileList::Numbers(ranges))
    }
}

fn file_number(text: &str) -> Result<u32, String> {
    match text.trim().parse::<u32>() {
        Ok(0) => Err("file numbers start at 1, as `npeer info` lists them".into()),
        Ok(number) => Ok(number),
        Err(_) => Err(format!(
            "`{}` is not a file number (example: 1,3,5-8)",
            text.trim()
        )),
    }
}

impl FileList {
    /// 0-based file indices, checked against the torrent's file count.
    fn indices(&self, file_count: usize) -> Result<Vec<u32>> {
        let ranges = match self {
            FileList::All => return Ok((0..file_count as u32).collect()),
            FileList::Numbers(ranges) => ranges,
        };
        let mut indices = Vec::new();
        for range in ranges {
            if *range.end() as usize > file_count {
                bail!(
                    "there is no file {}: this torrent has {file_count} file{} \
                     (see `npeer info`)",
                    range.end(),
                    if file_count == 1 { "" } else { "s" }
                );
            }
            indices.extend(range.clone().map(|number| number - 1));
        }
        Ok(indices)
    }
}

/// Which files of one torrent to download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChoice {
    Only(FileList),
    Skip(FileList),
}

impl FileChoice {
    /// Turn the choice into a selection, refusing ones with nothing to get.
    pub fn apply(&self, metainfo: &Metainfo) -> Result<FileSelection> {
        let count = metainfo.files.len();
        let selection = match self {
            FileChoice::Only(list) => FileSelection::only(count, &list.indices(count)?),
            FileChoice::Skip(list) => FileSelection::skipping(count, &list.indices(count)?),
        };
        if selection.wanted_count() == 0 {
            bail!("every file is skipped, so there is nothing to download");
        }
        if selection.selected_length(metainfo) == 0 {
            bail!("the chosen files are all empty, so there is nothing to download");
        }
        Ok(selection)
    }
}

/// 0-based indices shown as compact 1-based ranges: [0, 1, 2, 6] -> "1-3, 7".
pub fn describe(indices: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < indices.len() {
        let start = indices[index];
        let mut end = start;
        while index + 1 < indices.len() && indices[index + 1] == end + 1 {
            index += 1;
            end += 1;
        }
        parts.push(if start == end {
            format!("{}", start + 1)
        } else {
            format!("{}-{}", start + 1, end + 1)
        });
        index += 1;
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbers(input: &str) -> Vec<RangeInclusive<u32>> {
        match input.parse::<FileList>().unwrap() {
            FileList::Numbers(ranges) => ranges,
            FileList::All => panic!("expected numbers"),
        }
    }

    #[test]
    fn parses_numbers_and_ranges() {
        assert_eq!(numbers("3"), vec![3..=3]);
        assert_eq!(numbers("1,3,5-8"), vec![1..=1, 3..=3, 5..=8]);
        assert_eq!(numbers(" 2 - 4 , 9, "), vec![2..=4, 9..=9]);
        assert_eq!("ALL".parse::<FileList>().unwrap(), FileList::All);
    }

    #[test]
    fn rejects_bad_numbers_with_a_hint() {
        let error = |input: &str| input.parse::<FileList>().unwrap_err();
        assert!(error("0").contains("start at 1"));
        assert!(error("x").contains("not a file number"));
        assert!(error("8-5").contains("write 5-8"));
        assert!(error(",").contains("no file numbers"));
    }

    #[test]
    fn indices_are_zero_based_and_range_checked() {
        let list: FileList = "1,3-4".parse().unwrap();
        assert_eq!(list.indices(4).unwrap(), vec![0, 2, 3]);
        let error = list.indices(3).unwrap_err().to_string();
        assert!(error.contains("no file 4"), "got: {error}");
        assert!(error.contains("3 files"), "got: {error}");
        assert_eq!(FileList::All.indices(3).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn choices_must_leave_something_to_download() {
        let meta = Metainfo::from_info_bytes(&multi_file_info(&[5, 0, 7]), vec![]).unwrap();
        let skip_all = FileChoice::Skip(FileList::All);
        assert!(skip_all.apply(&meta).is_err());
        let only_empty = FileChoice::Only("2".parse().unwrap());
        assert!(only_empty.apply(&meta).is_err());

        let skip_last = FileChoice::Skip("3".parse().unwrap());
        assert_eq!(
            skip_last.apply(&meta).unwrap(),
            FileSelection::skipping(3, &[2])
        );
    }

    #[test]
    fn describes_indices_as_ranges() {
        assert_eq!(describe(&[0, 1, 2, 6]), "1-3, 7");
        assert_eq!(describe(&[16, 17]), "17-18");
        assert_eq!(describe(&[4]), "5");
        assert_eq!(describe(&[]), "");
    }

    /// Info dictionary with files of the given lengths, in pieces of 8.
    fn multi_file_info(lengths: &[u64]) -> Vec<u8> {
        let total: u64 = lengths.iter().sum();
        let pieces = vec![0u8; 20 * total.div_ceil(8) as usize];
        let mut info = b"d5:filesl".to_vec();
        for (index, length) in lengths.iter().enumerate() {
            info.extend_from_slice(format!("d6:lengthi{length}e4:pathl2:f{index}ee").as_bytes());
        }
        info.extend_from_slice(b"e4:name1:d12:piece lengthi8e");
        info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        info
    }
}
