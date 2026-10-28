pub struct Spring {
    pub value: f64,
    velocity: f64,
    target: f64,
    stiffness: f64,
    damping: f64,
    mass: f64,
}

impl Spring {
    pub fn new(value: f64, stiffness: f64, damping: f64) -> Self {
        Self {
            value,
            velocity: 0.0,
            target: value,
            stiffness,
            damping,
            mass: 1.0,
        }
    }

    pub fn set(&mut self, target: f64) {
        self.target = target;
    }

    pub fn snap(&mut self, target: f64) {
        self.target = target;
        self.value = target;
        self.velocity = 0.0;
    }

    /// Advance the spring. Returns true while it is still moving.
    pub fn update(&mut self, dt: f64) -> bool {
        let force = -self.stiffness * (self.value - self.target);
        let damp = -self.damping * self.velocity;
        let accel = (force + damp) / self.mass;
        self.velocity += accel * dt;
        self.value += self.velocity * dt;
        if (self.value - self.target).abs() < 0.05 && self.velocity.abs() < 0.05 {
            self.value = self.target;
            self.velocity = 0.0;
            false
        } else {
            true
        }
    }
}
