mod directory;
pub mod image_loader;
mod pending_requests;

use std::{
    borrow::Cow,
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs, mem,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::Ordering,
    time::SystemTime,
};

use gelatin::{
    glium::{
        self,
        texture::{MipmapsOption, RawImage2d, SrgbTexture2d},
        CapabilitiesSource,
    },
    image,
};
use log::trace;

use self::{
    directory::{DirItem, Directory},
    image_loader::*,
    pending_requests::PendingRequests,
};

pub mod errors {
    use std::io;

    use gelatin::{glium::texture, image};

    use crate::image_cache::image_loader;

    pub type Result<T = (), E = Error> = std::result::Result<T, E>;

    #[derive(Debug, thiserror::Error)]
    pub enum Error {
        #[error("Error during IO")]
        Io(#[from] io::Error),
        #[error(transparent)]
        TextureCreationError(#[from] texture::TextureCreationError),
        #[error(transparent)]
        ImageRsError(#[from] image::ImageError),
        #[error(transparent)]
        TextureLoaderError(#[from] image_loader::errors::Error),
        #[error(transparent)]
        DirError(#[from] super::directory::Error),
        #[error("ImageCache is waiting for loader to send result")]
        WaitingOnLoader,
        #[error("ImageCache is waiting for the directory items to be filtered for image files")]
        WaitingOnDirFilter,
        #[error("Failed to load {req_id}")]
        FailedToLoadImage { req_id: u32 },
        #[error("{0}")]
        Msg(String),
    }
}

pub use self::errors::Result;
use self::errors::*;

pub fn get_image_size_estimate(width: u32, height: u32) -> isize {
    // In an RGBA image, each pixel is 4 bytes.
    // counting all the mipmaps would add an additionnal multiplier of around ~1.6
    // but only the gpu textures have mip maps so just multiply by 1.5
    // 4 x 1.5 gives the factor 6.
    (width * height * 6) as isize
}

pub fn get_anim_size_estimate(frames: &[AnimationFrameTexture]) -> isize {
    frames
        .iter()
        .map(|frame| get_image_size_estimate(frame.w, frame.h))
        .sum()
}

/// The request sender function must process all prefetched requests to avoid
/// hitting the pending request limit (MAX_PENDING_REQUESTS). The display is needed
/// for this processing but it would be incorrect to require the dispaly for prefetch
/// requests
enum RequestKind<'a> {
    NonPriority,
    Priority { display: &'a glium::Display },
}

impl<'a> RequestKind<'a> {
    pub fn priority(self) -> bool {
        match self {
            RequestKind::Priority { .. } => true,
            RequestKind::NonPriority => false,
        }
    }
}

pub struct TextureGridItem {
    pub tex: SrgbTexture2d,
    pub col: u32,
    pub row: u32,
}

#[derive(Clone)]
pub struct AnimationFrameTexture {
    /// The maximum texture size supported by GPUs is limited. However it may be
    /// the I want to view a 16k*16k image while my GPU only supports 4k*4k
    /// textures. To work around this, we split up large images into a grid of
    /// smaller ones, which are displayed to appear as one continous surface.
    pub tex_grid: Rc<Vec<TextureGridItem>>,
    /// Number of physical pixels between two adjacent cells in one dimension.
    /// For example the pixel offset from the corner of the image to the corner
    /// of the cell at the 3rd column and 2nd row is
    /// (3*cell_step_size, 2*cell_step_size)
    pub cell_step_size: u32,
    pub grid_rows: u32,
    pub grid_cols: u32,

    pub delay_nano: u64,
    pub orientation: Orientation,

    /// The total width of the image. This equals to the sum of the widths of the
    /// textures from a single row of the grid
    pub w: u32,
    /// The total height of the image. This equals to the sum of the heights of the
    /// textures from a single column of the grid
    pub h: u32,
}

impl AnimationFrameTexture {
    pub fn from_image(
        display: &glium::Display,
        image: image::RgbaImage,
        delay_nano: u64,
        orientation: Orientation,
    ) -> Result<Self> {
        let (w, h) = image.dimensions();
        let img_bytes = image.into_raw();
        let mut tex_grid = Vec::new();

        // The reasoning behind dividing by 2 and taking the min with 4*1024, is
        // that if the textures are going to be swaped out from GPU memory it
        // might be easier to shuffle smaller chunks of memory around. (Because
        // I believe that if the memory is fragmented, it is easier to find
        // space for a smaller texture)
        let max_size = (display.get_capabilities().max_texture_size as u32 / 2)
            .min(4 * 1024);

        // DEBUG
        // let max_size = 1024;

        let grid_cols = ((w - 1) / max_size) + 1;
        let grid_rows = ((h - 1) / max_size) + 1;

        for row in 0..grid_rows {
            for col in 0..grid_cols {
                let offset_x = col * max_size;
                let offset_y = row * max_size;
                let cell_w = (w - offset_x).min(max_size);
                let cell_h = (h - offset_y).min(max_size);
                let tex = texture_from_img_rect(
                    display, w, h, &img_bytes, offset_x, offset_y, cell_w,
                    cell_h,
                )?;
                let item = TextureGridItem { tex, col, row };
                tex_grid.push(item);
            }
        }

        Ok(AnimationFrameTexture {
            tex_grid: Rc::new(tex_grid),
            delay_nano,
            orientation,
            w,
            h,
            cell_step_size: max_size,
            grid_rows,
            grid_cols,
        })
    }

    pub fn oriented_dimensions(&self) -> (u32, u32) {
        use Orientation::*;
        match self.orientation {
            Deg0 | Deg0HorFlip | Deg180 | Deg180HorFlip => (self.w, self.h),
            Deg90 | Deg90VerFlip | Deg270 | Deg270VerFlip => (self.h, self.w),
        }
    }
}

/// img_bytes has to be an rgba8 buffer.
#[allow(clippy::too_many_arguments)]
fn texture_from_img_rect(
    display: &glium::Display,
    img_w: u32,
    img_h: u32,
    img_bytes: &[u8],
    offset_x: u32,
    offset_y: u32,
    cell_w: u32,
    cell_h: u32,
) -> Result<SrgbTexture2d> {
    let raw_image = if img_w == cell_w {
        assert!(offset_x == 0);
        let start = (offset_y as usize * img_w as usize) * 4;
        let end = start + (cell_h as usize * cell_w as usize * 4);
        RawImage2d {
            data: Cow::Borrowed(&img_bytes[start..end]),
            format: glium::texture::ClientFormat::U8U8U8U8,
            width: cell_w,
            height: cell_h,
        }
    } else {
        let cell_size = cell_w as usize * cell_h as usize * 4;
        let mut cell_pixels = Vec::with_capacity(cell_size);
        for y in offset_y..(offset_y + cell_h) {
            // We multiply by four becase we need to convert from a pixel offset to
            // a byte offset and each pixel is 4 bytes wide.
            let start = (y as usize * img_w as usize + offset_x as usize) * 4;
            let end = start + (cell_w as usize * 4);
            cell_pixels.extend_from_slice(&img_bytes[start..end]);
        }
        RawImage2d::from_raw_rgba(cell_pixels, (cell_w, cell_h))
    };

    let x_pow = 31 - img_w.leading_zeros();
    let y_pow = 31 - img_h.leading_zeros();

    let max_mipmap_levels = x_pow.min(y_pow).min(4);

    let mipmaps = if max_mipmap_levels == 1 {
        MipmapsOption::NoMipmap
    } else {
        MipmapsOption::AutoGeneratedMipmapsMax(max_mipmap_levels)
        //MipmapsOption::AutoGeneratedMipmaps
    };
    Ok(SrgbTexture2d::with_mipmaps(display, raw_image, mipmaps)?)
}

struct CachedTexture {
    /// Contains the load request id
    _req_id: u32,
    needs_update: bool,
    mod_time: Option<SystemTime>,

    /// This is false if there are frames from the animation that haven't been added.
    /// This is used when requesting a frame that's outside of `frames`.
    /// In such a case the value of `fully_loaded` is inspected and if the image
    /// is loaded the frame number is wrapped around so that it falls onto the range.
    /// If it's not fully loaded yet a `WaitingOnLoader` error is returned.
    fully_loaded: bool,

    /// - `false` if loading is still in progress or if succeeded.
    /// - `true` if this failed to load,
    failed: bool,

    /// If the target file is an image this vector will have a single texture once the
    /// image uploaded to the GPU. If the target file is an animated image like a gif,
    /// these the frames
    frames: Vec<AnimationFrameTexture>,
}

/// The process of loading an image (or animation frame) consists of the following steps.
/// Note that even still images are handled as 1 frame long animations as there is
/// semantically no difference between those and this keeps the code relatively simple.
///
/// - ImageCache - Load request sent
///     - An entry representing the request is created in `ImageCache::ongoing_requests`
/// - Worker thread - Decodes an image from the byte stream into a CPU-side buffer. Send this back on a channel
///     - Decoded pixel data on channel (CPU)
/// - ImageCache - Prefetched image received from worker thread
///     - Decoded pixel data in `ImageCache::prefetched`
/// - ImageCache - Prefetched image is uploaded to a GPU texture from the CPU
///     - Pixel data is on the gpu referred to by `ImageCache::texture_cache`
///
pub struct ImageCache {
    dir: Directory,

    //current_name: OsString,
    //current_file_idx: usize,
    current_frame_idx: usize,

    remaining_capacity: isize,
    total_capacity: isize,
    curr_est_size: isize,

    pending_requests: PendingRequests,
    texture_cache: BTreeMap<u32, CachedTexture>,
    loader: ImageLoader,
}

/// This is a store for the supported images loaded from a folder
///
/// The basic idea is to have a few images already in the memory while an image is shown on the screen
impl ImageCache {
    const MAX_PENDING_REQUESTS: usize = 5;

    /// # Arguments
    /// * `capacity` - Number of bytes. The last image loaded will be the one at which the allocated memory reaches or exceeds capacity
    pub fn new(capacity: isize, threads: u32) -> ImageCache {
        ImageCache {
            dir: Directory::new(),
            //current_file_idx: 0,
            current_frame_idx: 0,

            remaining_capacity: capacity,
            total_capacity: capacity,
            curr_est_size: 1000, // 1 kb, an optimistic estimate for the image size before anything is loaded

            pending_requests: PendingRequests::new(),
            texture_cache: BTreeMap::new(),
            loader: ImageLoader::new(threads),
        }
    }

    pub fn current_filename(&self) -> Option<OsString> {
        self.dir.curr_filename()
    }

    pub fn current_file_path(&self) -> Option<PathBuf> {
        self.current_filename()
            .map(|name| self.dir.path().join(name))
    }

    /// Returns `None` when the directory hasn't finished filtering image files.
    pub fn current_file_index(&mut self) -> Option<usize> {
        self.dir.curr_img_index()
    }

    /// Returns `None` when the directory hasn't finished filtering image files.
    pub fn current_dir_len(&mut self) -> Option<usize> {
        self.dir.image_count()
    }

    fn curr_dir_item(&self) -> Result<DirItem> {
        if let Some(desc) = self.dir.curr_descriptor() {
            Ok(desc.clone())
        } else {
            Err(Error::Msg(
                "Could not get the current file descriptor".to_string(),
            ))
        }
    }

    /// Returns tru if and only if the current image has been fully loaded and it has a single frame.
    pub fn loaded_still_image(&self) -> bool {
        if let Some(desc) = self.dir.curr_descriptor() {
            if let Some(img) = self.texture_cache.get(&desc.request_id) {
                if img.fully_loaded && img.frames.len() == 1 {
                    return true;
                }
            }
        }
        false
    }

    /// Fetches the contents of the folder and stores the list of image filenames to know which
    /// files will be the next and previous.
    ///
    /// Tries to locate the image that was the current image before calling the function and
    /// keeping it current. If that filename is not found, than it tries to preserve the previous
    /// file index instead of the filename. If there is no such an index in the folder, it resets
    /// the index to 0 making the current file the first one in the folder.
    ///
    /// Returns the error that might occure while fetching the files from the directory. Otherwise
    /// returns `Ok(())`
    pub fn update_directory(&mut self) -> Result<()> {
        self.dir.update_directory()?;

        // indicate that the an update directory
        // call was made since those were created and they should all be
        // checked against the modification time of the file system file.
        for texture in self.texture_cache.values_mut() {
            texture.needs_update = true;
        }

        Ok(())
    }

    pub fn load_at_index(
        &mut self,
        display: &glium::Display,
        index: usize,
        frame_id: Option<isize>,
    ) -> Result<(AnimationFrameTexture, PathBuf)> {
        let path = self
            .dir
            .image_by_index(index)
            .ok_or_else(|| Error::WaitingOnDirFilter)?
            .path
            .clone();

        let result = self.load_specific(display, &path, frame_id)?;
        Ok((result, path))
    }

    /// Returns `Err(errors::Error::WaitingOnLoader)`
    /// when the image
    pub fn load_specific(
        &mut self,
        display: &glium::Display,
        path: &Path,
        frame_id: Option<isize>,
    ) -> Result<AnimationFrameTexture> {
        trace!("Begin `load_specific`");
        self.receive_prefetched();
        trace!("Receive prefetched done");
        let target_file_name;
        let parent;
        if path.is_dir() {
            parent = path.to_owned();
            target_file_name = None;
        } else {
            let filename_and_parent = get_file_name_and_parent(path)?;
            target_file_name = Some(filename_and_parent.0);
            parent = filename_and_parent.1;
        }

        let prev_img_index = self.dir.curr_img_index();
        if let Some(target_file_name) = target_file_name {
            self.change_directory_with_filename(&parent, &target_file_name)?;
        } else {
            self.change_directory(&parent)?;
            self.current_frame_idx = 0;
        }
        if self.dir.path() != parent {
            let DirItem { path, request_id } = self.curr_dir_item()?;
            self.send_request_for_file(
                path,
                request_id,
                RequestKind::Priority { display },
            );
            return Err(errors::Error::WaitingOnLoader);
        }
        if let Some(img_index) = self.dir.curr_img_index() {
            self.dir.set_curr_img_index(img_index)?;
        }
        let requested_frame_id = match frame_id {
            Some(frame_id) => frame_id,
            None => {
                let mut retval = 0;
                if let Some(prev_img_index) = prev_img_index {
                    if let Some(curr_img_index) = self.dir.curr_img_index() {
                        if curr_img_index == prev_img_index {
                            retval = self.current_frame_idx as isize;
                        }
                    }
                }
                retval
            }
        };
        self.refresh_cache();
        self.try_getting_requested_image(display, requested_frame_id)
    }

    fn refresh_cache(&mut self) {
        trace!("Begin `refresh_cache`");
        if let Some(curr_index) = self.dir.curr_img_index() {
            let cache = mem::take(&mut self.texture_cache);

            // Delete all entries that are outside the range of files around the current file
            // allowed by the capacity.
            // Walk through our list of directory entries sorted by their distance from the current
            // file and in each step remove an entry from the cache until we reach the desired cache
            // size
            let mut sorted_files: Vec<_> =
                cache.into_iter().enumerate().collect();
            sorted_files.sort_unstable_by_key(|&(index, _)| {
                (index as isize - curr_index as isize).abs()
            });
            self.remaining_capacity = self.total_capacity;
            sorted_files.retain(|(_, (_, texture))| {
                // TODO consider retaining individual frames.
                let all_frames_size = get_anim_size_estimate(&texture.frames);

                if self.remaining_capacity
                    > (all_frames_size + self.curr_est_size)
                {
                    self.remaining_capacity -= all_frames_size;
                    true
                } else {
                    false
                }
            });

            self.texture_cache =
                sorted_files.into_iter().map(|(_, entry)| entry).collect();
        }
    }

    pub fn load_next(
        &mut self,
        display: &glium::Display,
    ) -> Result<(AnimationFrameTexture, PathBuf)> {
        self.load_jump(display, 1, 0)
    }

    pub fn load_prev(
        &mut self,
        display: &glium::Display,
    ) -> Result<(AnimationFrameTexture, PathBuf)> {
        self.load_jump(display, -1, 0)
    }

    pub fn load_jump(
        &mut self,
        display: &glium::Display,
        file_jump_count: i32,
        frame_jump_count: isize,
    ) -> Result<(AnimationFrameTexture, PathBuf)> {
        if file_jump_count == 0 {
            // Here, it is possible that the current image was already
            // requested but not yet loaded.
            let target_frame =
                self.current_frame_idx as isize + frame_jump_count;
            let requested =
                self.try_getting_requested_image(display, target_frame);
            if let Some(path) = self.current_file_path() {
                return requested.map(|t| (t, path));
            } else {
                return Err(Error::Msg("No file is open".to_string()));
            }
        } else {
            self.current_frame_idx = 0;
        }

        let target_path;
        if file_jump_count.abs() == 1 {
            if file_jump_count > 0 {
                self.dir.jump_to_next();
            } else {
                self.dir.jump_to_prev();
            }
            target_path = self.dir.curr_descriptor().unwrap().path.clone();
        } else if let (Some(curr_index), Some(img_count)) =
            (self.dir.curr_img_index(), self.dir.image_count())
        {
            // rem_euclid calculates the least nonnegative remainder
            let target_index = (curr_index as isize + file_jump_count as isize)
                .rem_euclid(img_count as isize)
                as usize;

            target_path =
                self.dir.image_by_index(target_index).unwrap().path.clone();
        } else {
            return Err(Error::Msg("Folder is empty, no folder was open, or folder hasn't finished filtering when trying to jump to an image by index.".to_string()));
        }
        let result = self.load_specific(display, &target_path, None)?;
        Ok((result, target_path))
    }

    fn receive_prefetched(&mut self) {
        use std::sync::mpsc::TryRecvError;
        loop {
            match self.loader.try_recv_prefetched() {
                Ok(load_result) => {
                    self.pending_requests.add_load_result(load_result);
                }
                Err(TryRecvError::Disconnected) => {
                    panic!("Channel disconnected unexpectidly.")
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }

    pub fn process_prefetched(
        &mut self,
        display: &glium::Display,
    ) -> Result<()> {
        self.receive_prefetched();
        let mut uploaded_one = false;
        let req_ids = self.pending_requests.get_all_ids();
        for id in req_ids {
            if let Some(results) = self.pending_requests.take_results(id) {
                for result in results {
                    match self.upload_to_texture(display, result) {
                        Ok(_) => uploaded_one = true,
                        // it's okay to ignore if the image falied to load here, this is just pre-fetch.
                        Err(Error::FailedToLoadImage { .. }) => {}
                        Err(e) => return Err(e),
                    }
                }
            }

            if uploaded_one {
                break;
            }
        }
        Ok(())
    }

    /// This funciton will check if the image which we are at, is already avaialbe.
    ///
    ///
    ///
    /// Negative frame numbers are allowed. So are larger-than-total-frame-count frame numbers.
    ///
    /// This is because this function call will often happen when the animation is not fully loaded yet.
    /// So in order to minimize complexity of the functions calling this one, frame ids that are out of
    /// bounds are allowed and will be wraped around if needed within this function.
    fn try_getting_requested_image(
        &mut self,
        display: &glium::Display,
        frame_id: isize,
    ) -> Result<AnimationFrameTexture> {
        trace!("Begin `try_getting_requested_image` in `image_cache`");
        let DirItem {
            path,
            request_id: req_id,
        } = self.curr_dir_item()?;

        // Check if it's among the prefetched, and upload it, if it is
        if let Some(results) = self.pending_requests.take_results(req_id) {
            for load_result in results {
                self.upload_to_texture(display, load_result)?;
            }
            // And just let the next blok deal with locating the appropriate frame.
        }

        // Check if it is inside the texture cache first
        if let Some(tex) = self.texture_cache.get(&req_id) {
            if tex.failed {
                return Err(Error::FailedToLoadImage { req_id });
            }
            let modified =
                fs::metadata(&path).ok().and_then(|m| m.modified().ok());
            let mut get_from_cache = false;
            if let Some(curr_mod_time) = modified {
                if let Some(mod_time) = tex.mod_time {
                    if mod_time == curr_mod_time {
                        get_from_cache = true;
                    }
                }
            } else {
                get_from_cache = true;
            }
            if get_from_cache {
                let count = tex.frames.len() as isize;
                if tex.fully_loaded || (frame_id >= 0 && frame_id < count) {
                    let wrapped_id = if frame_id < 0 {
                        count + (frame_id % count)
                    } else {
                        frame_id % count
                    };
                    if let Some(frame) = tex.frames.get(wrapped_id as usize) {
                        self.current_frame_idx = wrapped_id as usize;
                        return Ok(frame.clone());
                    }
                }
            }

            return Err(Error::WaitingOnLoader);
        }
        if self.pending_requests.contains(&req_id) {
            PRIORITY_REQUEST_ID.store(req_id, Ordering::SeqCst);

            return Err(Error::WaitingOnLoader);
        }
        self.send_request_for_file(
            path,
            req_id,
            RequestKind::Priority { display },
        );
        // If the texture is not in the cache just throw our hands in the air
        // and tell the caller that we gotta wait for the loader to load this texture.
        Err(Error::WaitingOnLoader)
    }

    fn upload_to_texture(
        &mut self,
        display: &glium::Display,
        load_result: LoadResult,
    ) -> Result<Option<AnimationFrameTexture>> {
        use std::collections::btree_map::Entry;
        match load_result {
            LoadResult::Start { req_id, metadata } => {
                let curr_mod_time = metadata.modified().ok();
                if let Some(cancelled) =
                    self.pending_requests.cancelled(&req_id)
                {
                    if cancelled {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                }
                match self.texture_cache.entry(req_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(CachedTexture {
                            _req_id: req_id,
                            needs_update: false,
                            fully_loaded: false,
                            mod_time: curr_mod_time,
                            failed: false,
                            frames: Vec::new(),
                        });
                    }
                    Entry::Occupied(mut entry) => {
                        let mut overwrite = true;
                        if let Some(curr_mod_time) = curr_mod_time {
                            let cached = entry.get();
                            if let Some(existing_mod_time) = cached.mod_time {
                                if existing_mod_time == curr_mod_time {
                                    overwrite = false;
                                }
                            }
                        }
                        if overwrite {
                            let old_size_estimate =
                                get_anim_size_estimate(&entry.get().frames);
                            self.remaining_capacity += old_size_estimate;
                            let mut_entry = entry.get_mut();
                            mut_entry.frames.clear();
                            mut_entry.mod_time = curr_mod_time;
                        }
                    }
                }
                Ok(None)
            }
            LoadResult::Frame {
                req_id,
                image,
                delay_nano,
                orientation,
            } => {
                if let Some(cancelled) =
                    self.pending_requests.cancelled(&req_id)
                {
                    if cancelled {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                }
                let size_estimate =
                    get_image_size_estimate(image.width(), image.height());
                if let Some(entry) = self.texture_cache.get_mut(&req_id) {
                    let anim_frame = AnimationFrameTexture::from_image(
                        display,
                        image,
                        delay_nano,
                        orientation,
                    )?;
                    entry.frames.push(anim_frame.clone());
                    self.remaining_capacity -= size_estimate;
                    return Ok(Some(anim_frame));
                }
                Ok(None)
            }
            LoadResult::Done { req_id } => {
                if let Some(tex) = self.texture_cache.get_mut(&req_id) {
                    tex.fully_loaded = true;
                }
                let _ = PRIORITY_REQUEST_ID.compare_exchange(
                    req_id,
                    NON_EXISTENT_REQUEST_ID,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                self.pending_requests.set_finished(&req_id);
                Ok(None)
            }
            LoadResult::Failed { req_id } => {
                if let Some(tex) = self.texture_cache.get_mut(&req_id) {
                    tex.fully_loaded = true;
                    tex.failed = true;
                }
                let _ = PRIORITY_REQUEST_ID.compare_exchange(
                    req_id,
                    NON_EXISTENT_REQUEST_ID,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                self.pending_requests.set_finished(&req_id);
                Err(errors::Error::FailedToLoadImage { req_id })
            }
        }
    }

    pub fn prefetch_neighbors(&mut self) {
        if let Some(mut index) = self.dir.curr_img_index() {
            // Send enough load requests so that the estimated total will just fill the cache
            let mut estimated_remaining_cap = self.remaining_capacity;

            while estimated_remaining_cap > self.curr_est_size {
                // Send a load request for the closest file not in the cache or outdated
                index += 1;
                if self.prefetch_at_index(index) {
                    estimated_remaining_cap -= self.curr_est_size;
                } else {
                    break;
                }
            }
        }
    }

    pub fn prefetch_at_index(&mut self, index: usize) -> bool {
        if self.remaining_capacity > self.curr_est_size {
            let params = self
                .dir
                .image_by_index(index)
                .map(|desc| (desc.path.clone(), desc.request_id));
            if let Some((path, req_id)) = params {
                return self.send_request_for_file(
                    path,
                    req_id,
                    RequestKind::NonPriority,
                );
            } else {
                return false;
            }
        }
        false
    }

    /// This is almost identical to `prefetch_at_index` but this function
    /// does not check the `remaining_capacity`.
    fn send_request_for_file(
        &mut self,
        file_path: PathBuf,
        req_id: u32,
        kind: RequestKind,
    ) -> bool {
        if let RequestKind::Priority { display } = kind {
            if self.pending_requests.len() >= Self::MAX_PENDING_REQUESTS {
                if let Err(e) = self.process_prefetched(display) {
                    eprintln!(
                        "Error while processing prefetched images:\n{}",
                        e
                    );
                }
            }
        }
        if self.pending_requests.len() >= Self::MAX_PENDING_REQUESTS {
            return false;
        }
        let mut cache_enty_invalid = false;
        if let Some(texture) = self.texture_cache.get_mut(&req_id) {
            if !texture.needs_update {
                return false;
            } else {
                texture.needs_update = false;
                if let Some(existing_mod_time) = texture.mod_time {
                    let new_mod_time = fs::metadata(&file_path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                    if let Some(new_mod_time) = new_mod_time {
                        if new_mod_time == existing_mod_time {
                            return false;
                        } else {
                            cache_enty_invalid = true;
                        }
                    }
                }
            }
        }
        if cache_enty_invalid {
            self.texture_cache.remove(&req_id);
        }
        if kind.priority() {
            PRIORITY_REQUEST_ID.store(req_id, Ordering::SeqCst);
        }
        if self.pending_requests.contains(&req_id) {
            return false;
        }
        let request = LoadRequest {
            req_id,
            path: file_path,
        };
        self.pending_requests.add_request(request.clone());
        self.loader.send_load_request(request);
        true
    }

    fn change_directory(&mut self, dir_path: &Path) -> Result<()> {
        if self.dir.path() == dir_path {
            return Ok(());
        }
        self.texture_cache.clear();
        self.remaining_capacity = self.total_capacity;

        // Cancel all pending load requests
        for (_, request) in self.pending_requests.iter_mut() {
            request.cancel();
        }

        self.dir.change_directory(dir_path)?;
        Ok(())
    }

    fn change_directory_with_filename(
        &mut self,
        dir_path: &Path,
        filename: &OsStr,
    ) -> Result<()> {
        self.dir
            .change_directory_with_filename(dir_path, filename)
            .map_err(|e| Error::Msg(format!("{e}")))
    }

    // fn collect_directory(&mut self) -> Result<Vec<DirItem>> {
    // 	let start = std::time::Instant::now();
    // 	let mut dir_files: Vec<_> = fs::read_dir(&self.dir.path)?
    // 		.filter_map(|x| match x {
    // 			Ok(entry) => match entry.file_type() {
    // 				Ok(file_type) => {
    // 					if file_type.is_file() || file_type.is_symlink() {
    // 						if is_file_supported(entry.path().as_path()) {
    // 							self.current_req_id += 1;
    // 							Some(DirItem { dir_entry: entry, request_id: self.current_req_id })
    // 						} else {
    // 							None
    // 						}
    // 					} else {
    // 						None
    // 					}
    // 				}
    // 				Err(_) => None,
    // 			},
    // 			Err(_) => None,
    // 		})
    // 		.collect();

    // 	let total_time = start.elapsed();
    // 	println!("Collected directory in: {} ms", total_time.as_millis());
    // 	dir_files.sort_unstable_by(|a, b| {
    // 		lexical_sort::natural_lexical_cmp(
    // 			&a.dir_entry.file_name().to_string_lossy(),
    // 			&b.dir_entry.file_name().to_string_lossy(),
    // 		)
    // 	});

    // 	Ok(dir_files)
    // }
}

fn get_file_name_and_parent(path: &Path) -> Result<(OsString, PathBuf)> {
    let file_name = match path.file_name() {
        Some(f) => f.to_owned(),
        None => {
            return Err(Error::Msg(format!(
                "Could not get file name from path {path:?}"
            )))
        }
    };
    let parent = match path.parent() {
        Some(p) => {
            if p == Path::new("") {
                Path::new(".").canonicalize()?
            } else {
                p.canonicalize()?
            }
        }
        None => {
            let mut path = path.canonicalize()?;
            if !path.pop() {
                return Err(Error::Msg(format!(
                    "Could not get parent directory of {path:?}"
                )));
            }
            path
        }
    };

    Ok((file_name, parent))
}
