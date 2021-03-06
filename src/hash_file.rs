use helpers::{create_or_open_file, hash, get_size};
use helpers::{read_one_line, seek_from_start, write_buffer};

use {SEP};

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter};
use std::mem;
use std::ops::AddAssign;
use std::str::FromStr;

const TEMP_SUFFIX: &'static str = ".hash_file";
const DAT_SUFFIX: &'static str = ".dat";

// FIXME: have a bool for marking key/vals to be removed (required for `remove` method)
struct KeyIndex<K: Display + FromStr + Hash> {
    key: K,
    idx: u64,
    count: usize,
}

impl<K: Display + FromStr + Hash> KeyIndex<K> {
    pub fn new(key: K) -> KeyIndex<K> {
        KeyIndex {
            key: key,
            idx: 0,
            count: 0,
        }
    }
}

// FIXME: This should be changed to serialization
impl<K: Display + FromStr + Hash> Display for KeyIndex<K> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{}{}{}{}", self.key, SEP, self.idx, SEP, self.count)
    }
}

impl<K: Display + FromStr + Hash> FromStr for KeyIndex<K> {
    type Err = String;

    fn from_str(s: &str) -> Result<KeyIndex<K>, String> {
        let mut split = s.split(SEP);
        Ok(KeyIndex {
            key: try!(split.next().unwrap_or("")
                                  .parse::<K>()
                                  .map_err(|_| format!("Cannot parse the key!"))),
            idx: try!(split.next().unwrap_or("")
                                  .parse::<u64>()
                                  .map_err(|_| format!("Cannot parse the index!"))),
            count: try!(split.next().unwrap_or("")
                                    .parse::<usize>()
                                    .map_err(|_| format!("Cannot parse 'overwritten' count"))),
        })
    }
}

impl<K: Display + FromStr + Hash> AddAssign for KeyIndex<K> {
    fn add_assign(&mut self, other: KeyIndex<K>) {
        self.idx = other.idx;
        self.count += 1;
    }
}

impl<K: Display + FromStr + Hash> Hash for KeyIndex<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

/// An implementation of a "file-based" map which stores key-value pairs in sorted fashion in
/// file(s), and gets them using binary search and file seeking in O(log-n) time.
///
/// # Design
///
/// `HashFile` maintains two files - one for the keys and value indices, and the other
/// for the values themselves. Since it uses padding to maintain constant line length
/// throughout the file, large values can lead to extremely huge and sparse files. Having two
/// files eliminates this problem.
///
/// - During insertion, the hash of the supplied key is obtained (using the built-in
/// [`SipHasher`][hasher]), which acts as the key for sorting.
///
/// - While flushing, two iterators (one for the key/values in the underlying map, another
/// for the keys in the file) and two writers (one for key-index, another for the values)
/// are maintained. The readers throw key/values in ascending order, the hashes of the keys
/// are compared, the values are appended to a temporary file, and the keys (along with the
/// value indices) are written to another temporary file. In the end, both the files are renamed.
///
/// - Basically, the files are in DSV format - one has keys and value indices separated by a
/// null byte, while the other has values separated by `\n`. Each line in the "key" file is
/// ensured to have the same length, by properly padding it with null bytes, which is done by
/// calling the [`finish`][finish] method, which also does a cleanup and gets rid of unnecessary
/// values from the "data" file.
///
/// - While getting, the hash for the given key is computed, and a [binary search][search]
/// is made by seeking through the file. The value index is found in O(log-n) time, and the value
/// is obtained from the "data" file in O(1) time.
///
/// # Examples
///
/// Once you've added the package to your `Cargo.toml`
///
/// ``` toml
/// catalog = "0.1.1"
/// ```
///
/// ... it can be used as follows,
///
/// ``` rust
/// extern crate catalog;
///
/// use catalog::HashFile;
///
/// // This will create a new file in the path (if it doesn't exist)
/// let mut hash_file: HashFile<usize, _> =
///     try!(HashFile::new("/tmp/SAMPLE").map(|hf| hf.set_capacity(100)));
///
/// // We don't have to mention all the types explicitly, leaving it to type inference
/// // But, there's a reason why I mentioned `usize` (which we'll see in a moment).
///
/// // Insert some stuff into the map (in this case, integers and upper case alphabets)
/// for i in 0..1000 {
///     try!(hf.insert(i, format!("{}", (65 + (i % 26)) as u8 as char)));
/// }
///
/// // This flushes the data to the file for every 100 key/value pairs, since
/// // we've set the capacity to 100.
///
/// // Call the finish method once you're done with insertion. It's necessary
/// // because it pads each line and ensures that all the lines have the same length.
/// try!(hf.finish());
///
/// // Now, we're ready to "get" the values.
/// let value = try!(hf.get(&0));
/// assert_eq!(Some(("A".to_owned(), 0)), value);
/// // Note that in addition to the value, there's a number.
///
/// // Let's try it again...
/// try!(hf.insert(0, format!("Z")));
/// // Don't forget to flush it to the file!
/// try!(hf.finish());
///
/// let value = try!(hf.get(&0));
/// assert_eq!(Some(("Z".to_owned(), 1)), value);
///
/// // So, the number is just a counter. HashFile keeps track of the number of
/// // times a value has been overridden (with insertion).
/// ```
///
/// Now, let's have a quick peek inside the generated file.
///
/// ``` bash
/// $ head -5 /tmp/SAMPLE*
/// ==> /tmp/SAMPLE <==
/// 68600
/// 18320
/// 59540
/// 50060
/// 1580
///
/// ==> /tmp/SAMPLE.dat <==
/// K
/// B
/// X
/// G
/// P
/// $ wc -l /tmp/SAMPLE*
///  1000 /tmp/SAMPLE
///  1000 /tmp/SAMPLE.dat
///  2000 total
/// $ ls -l /tmp/SAMPLE*
/// -rw-rw-r-- 1 user user 11000 Jul 09 22:10 /tmp/SAMPLE
/// -rw-rw-r-- 1 user user 2000 Jul 09 22:10 /tmp/SAMPLE.dat
/// ```
///
/// The file sizes will (and should!) always be a multiple of the number of
/// key/value pairs, since each line is padded to have the same length.
/// Now, we can have another program to get the key/value pairs.
///
/// ``` rust
/// // This will open the file in the path (if it exists)
/// let mut hf: HashFile<usize, String> = try!(HashFile::new("/tmp/SAMPLE"));
/// ```
///
/// A couple of things to note here. Before getting, we need to mention the types,
/// because `rustc` doesn't know what type we have in the file (and, it'll throw an error).
///
/// Moreover, if we hadn't explicitly mentioned `usize` during insertion,
/// `rustc` would've gone for some default type, and if we mention some other primitive
/// now, the hashes won't match i.e., `hash(0u32) != hash(0usize)`.
///
/// For example, `"2"` can be parsed to all the integer primitives (`u8`, `u64`, `isize`, etc.),
/// but, they all produce different hashes. In such a case, it's more likely that `HashFile`
/// returns `None` while getting the value corresponding to a key, even if it exists in
/// the file. Hence, it's up to the user to handle such cases
/// (by manually denoting the type during insertion and getting).
///
/// ``` rust
/// // Now, we can get the values...
/// let value = try!(hf.get(&0));
/// assert_eq!(Some(("Z".to_owned(), 1)), value);
///
/// // ... as many as we want!
/// let value = try!(hf.get(&1));
/// assert_eq!(Some(("B".to_owned(), 0)), value);
/// ```
///
/// We've used a lot of `try!` here, because each method invocation involves making OS
/// calls for manipulating the underlying file descriptor. Since all the methods have been
/// ensured to return a [`Result<T, E>`][result], `HashFile` can be guaranteed from
/// panicking along the run.
///
/// # Advantages:
/// - **Control over memory:** You're planning to put a great deal of "stuff" into a map, but you
/// cannot afford the memory it demands. You wanna have control on how much memory your map can
/// consume. That said, you still want a map which can throw the values for your requested keys in
/// appreciable time.
///
/// - **Long term storage:** You're sure that the large "stuff" won't change in the near future,
/// and so you're not willing to risk the deallocation (when the program quits) or re-insertion
/// (whenever the program starts).
///
/// # Drawbacks:
/// - **Sluggish insertion:** Re-allocation in memory is lightning fast, while putting stuff into
///  the usual maps, and so it won't be obvious during the execution of a program. But, that's not
/// the case when it comes to file. Flushing to a file takes time (as it makes OS calls), and it
/// increases exponentially as O(2<sup>n</sup>) during insertion, which would be *very* obvious
/// in our case.
///
/// [finish]: #method.finish
/// [hasher]: https://docs.rs/siphasher/%5E0.2/siphasher/sip/struct.SipHasher.html
/// [result]: https://doc.rust-lang.org/std/result/enum.Result.html
/// [search]: https://en.wikipedia.org/wiki/Binary_search_algorithm
pub struct HashFile<K: Display + FromStr + Hash, V: Display + FromStr> {
    file: File,
    path: String,
    size: u64,
    data_file: File,
    data_path: String,
    data_idx: u64,
    hashed: BTreeMap<u64, (KeyIndex<K>, V)>,
    capacity: usize,
    line_length: usize,
}

impl<K: Display + FromStr + Hash, V: Display + FromStr> HashFile<K, V> {
    /// Create a new `HashFile` for mapping key/value pairs in the given path.
    ///
    /// ``` rust
    /// let mut hf = try!(HashFile::new("/tmp/SAMPLE"));
    /// ```
    ///
    /// This will maintain two files - `SAMPLE` and `SAMPLE.dat` in `/tmp/`.
    /// The latter has the values, while the former has the keys (sorted by its hash)
    /// along with the value indices and overwritten count.
    pub fn new(path: &str) -> Result<HashFile<K, V>, String> {
        let mut file = try!(create_or_open_file(&path));
        let file_size = get_size(&file).unwrap_or(0);
        let data_path = format!("{}{}", path, DAT_SUFFIX);

        Ok(HashFile {
            hashed: BTreeMap::new(),
            capacity: 0,
            line_length: match file_size > 0 {
                true => {
                    let line = try!(read_one_line(&mut file));
                    line.len()
                },
                false => 0,
            },
            file: {
                try!(seek_from_start(&mut file, 0));
                file
            },
            data_file: try!(create_or_open_file(&data_path)),
            data_path: data_path,
            data_idx: 0,
            path: path.to_owned(),
            size: file_size,
        })
    }

    /// Set the capacity of the `HashFile` (to flush to the file whenever it exceeds this value).
    /// Since the constructor returns a `Result`, we can chain this method like so...
    ///
    /// ``` rust
    /// let mut hf = try!(HashFile::new("/tmp/SAMPLE").map(|hf| hf.set_capacity(1000)));
    /// ```
    ///
    /// Note that `HashFile` flushes for every insertion by default, which is pretty inefficient.
    /// Hence, setting a proper capacity (depending on the usage) is necessary.
    pub fn set_capacity(mut self, capacity: usize) -> HashFile<K, V> {
        self.capacity = capacity;
        self
    }

    fn rename_temp_file(&mut self, rename_dat: bool) -> Result<(), String> {
        if rename_dat {
            try!(fs::rename(format!("{}{}", &self.data_path, TEMP_SUFFIX), &self.data_path)
                    .map_err(|e| format!("Cannot rename the temp data file! ({})", e.description())));
            self.data_file = try!(create_or_open_file(&self.data_path));
        }

        try!(fs::rename(format!("{}{}", &self.path, TEMP_SUFFIX), &self.path)
                .map_err(|e| format!("Cannot rename the temp file! ({})", e.description())));
        self.file = try!(create_or_open_file(&self.path));
        self.size = try!(get_size(&self.file));
        Ok(())
    }

    /// Run this finally to flush the values (if any) from the struct to the file.
    ///
    /// ``` rust
    /// let mut hf = try!(HashFile::new("/tmp/SAMPLE").map(|hf| hf.set_capacity(100)));
    ///
    /// for i in 97..123 {
    ///     try!(hf.insert(i, format!("{}", i as char)));
    /// }
    ///
    /// try!(hf.finish());
    /// ```
    ///
    /// This method is essential, because the "key" file would otherwise be invalid. This
    /// method ensures a constant line length throughout the file by properly padding them
    /// whenever required. It also gets rid of the unnecessary values from the "data" file.
    pub fn finish(&mut self) -> Result<(), String> {
        if self.hashed.len() > 0 {
            try!(self.flush_map());
        }

        // We need to traverse one last time to confirm that each row has the same length
        // (and of course, remove the *removed* values from the data file)

        {
            let buf_reader = BufReader::new(&mut self.file);
            let mut out_file = try!(create_or_open_file(&format!("{}{}", &self.path, TEMP_SUFFIX)));
            let mut buf_writer = BufWriter::new(&mut out_file);
            let mut data_file = try!(create_or_open_file(&format!("{}{}", &self.data_path, TEMP_SUFFIX)));
            let mut data_writer = BufWriter::new(&mut data_file);
            self.data_idx = 0;

            for ref line in buf_reader.lines().filter_map(|l| l.ok()) {
                let mut key_index = try!(line.parse::<KeyIndex<K>>());
                try!(seek_from_start(&mut self.data_file, key_index.idx));
                let value = try!(read_one_line(&mut self.data_file));
                key_index.idx = self.data_idx;
                self.data_idx += try!(write_buffer(&mut data_writer, &value, &mut 0));
                // Even though this takes a mutable reference, we can be certain that
                // we've found the maximum row length for this session
                try!(write_buffer(&mut buf_writer, &key_index.to_string(), &mut self.line_length));
            }
        }

        self.rename_temp_file(true)
    }

    fn flush_map(&mut self) -> Result<(), String> {
        let map = mem::replace(&mut self.hashed, BTreeMap::new());

        {
            // seeking, so that we can ensure that the cursor is at the start while reading,
            // and at the end while writing.
            try!(seek_from_start(&mut self.file, 0));
            try!(seek_from_start(&mut self.data_file, self.data_idx));

            let buf_reader = BufReader::new(&mut self.file);
            let mut out_file = try!(create_or_open_file(&format!("{}{}", &self.path, TEMP_SUFFIX)));
            let mut buf_writer = BufWriter::new(&mut out_file);
            let mut data_writer = BufWriter::new(&mut self.data_file);

            // both the iterators throw the values in ascending order
            let mut file_iter = buf_reader.lines().filter_map(|l| l.ok()).peekable();
            let mut map_iter = map.into_iter().peekable();

            loop {
                let compare_result = match (file_iter.peek(), map_iter.peek()) {
                    (Some(file_line), Some(&(btree_key_hash, _))) => {
                        let key = file_line.split(SEP).next().unwrap();
                        let file_key_hash = match key.parse::<K>() {
                            Ok(k_v) => hash(&k_v),
                            Err(_) => {
                                // skip the line if we find any errors
                                // (we don't wanna stop the giant build because of this error)
                                continue
                            },
                        };

                        file_key_hash.cmp(&btree_key_hash)
                    },
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => break,
                };

                match compare_result {
                    Ordering::Equal => {
                        let file_line = file_iter.next().unwrap();
                        let (_, (mut btree_key, val)) = map_iter.next().unwrap();
                        btree_key.idx = self.data_idx;
                        self.data_idx += try!(write_buffer(&mut data_writer, &val.to_string(), &mut 0));

                        let mut file_key = match file_line.parse::<KeyIndex<K>>() {
                            Ok(k_i) => k_i,
                            Err(_) => continue,     // skip on error
                        };

                        file_key += btree_key;
                        try!(write_buffer(&mut buf_writer, &file_key.to_string(),
                                          &mut self.line_length));
                    },
                    Ordering::Less => {
                        try!(write_buffer(&mut buf_writer, &file_iter.next().unwrap(),
                                          &mut self.line_length));
                    },
                    Ordering::Greater => {
                        let (_, (mut btree_key, val)) = map_iter.next().unwrap();
                        btree_key.idx = self.data_idx;
                        self.data_idx += try!(write_buffer(&mut data_writer, &val.to_string(), &mut 0));

                        try!(write_buffer(&mut buf_writer, &(btree_key.to_string()),
                                          &mut self.line_length));
                    },
                }
            }
        }

        self.rename_temp_file(false)
    }

    /// Insert a key/value pair into the map.
    ///
    /// ``` rust
    /// let mut hf = try!(HashFile::new("/tmp/SAMPLE").map(|hf| hf.set_capacity(1000)));
    ///
    /// try!(hf.insert("foo", "bar"));
    /// ```
    ///
    /// Note that this method flushes the stuff to the files once the map reaches the
    /// defined capacity.
    pub fn insert(&mut self, key: K, value: V) -> Result<(), String> {
        let mut key_idx = KeyIndex::new(key);
        let hashed = hash(&key_idx);
        if let Some(key_val) = self.hashed.get_mut(&hashed) {
            *key_val = {
                key_idx.count += 1;
                (key_idx, value)
            };

            return Ok(())
        }

        self.hashed.insert(hashed, (key_idx, value));
        if self.hashed.len() > self.capacity {  // flush to file once the capacity is full
            try!(self.flush_map());
        }

        Ok(())
    }

    /// Get the value corresponding to the key from the map.
    ///
    /// ``` rust
    /// let mut hf = try!(HashFile::new("/tmp/SAMPLE").map(|hf| hf.set_capacity(1000)));
    /// try!(hf.insert(0, "foo"));
    /// try!(hf.insert(1, "bar"));
    /// try!(hf.finish());      // don't forget to finish!
    ///
    /// let value = try!(hf.get(&0));
    /// assert_eq!(Some(("foo", 0)), value);
    /// ```
    ///
    /// Unlike the `get` methods of other maps, this takes a mutable reference, because it
    /// needs read access to the underlying file descriptors, as it moves the cursor here and
    /// there to read the key/value pairs.
    ///
    /// Also, note that this method only gets the stuff from the file. So, it's necessary
    /// that we've already flushed whatever we have on hand (by calling `finish`).
    pub fn get(&mut self, key: &K) -> Result<Option<(V, usize)>, String> {
        let hashed_key = hash(key);
        if self.size == 0 || try!(get_size(&self.data_file)) == 0 {
            return Ok(None)
        }

        let row_length = (self.line_length + 1) as u64;
        let mut low = 0;
        let mut high = self.size;

        // Binary search and file seeking to find the value(s)

        while low <= high {
            let mid = (low + high) / 2;
            // place the cursor at the start of a line
            let new_line_pos = mid - (mid + row_length) % row_length;
            try!(seek_from_start(&mut self.file, new_line_pos));
            let line = try!(read_one_line(&mut self.file));

            // we'll only need the hash of the key
            let mut split = line.split(SEP);
            let key_str = split.next().unwrap();
            let key = try!(key_str.parse::<K>()
                                  .map_err(|_| format!("Cannot parse the key from file!")));
            let hashed = hash(&key);

            if hashed == hashed_key {
                let key_index = try!(line.parse::<KeyIndex<K>>());
                try!(seek_from_start(&mut self.data_file, key_index.idx));
                let line = try!(read_one_line(&mut self.data_file));
                let value = try!(line.parse::<V>()
                                     .map_err(|_| format!("Cannot parse the value from file!")));
                return Ok(Some((value, key_index.count)))
            } else if hashed < hashed_key {
                low = mid + 1;
            } else {
                high = mid - 1;
            }
        }

        Ok(None)
    }
}
