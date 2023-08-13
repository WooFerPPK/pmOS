export class Draggable {
    constructor(element, zIndexManager, bar) {
        this.element = element;
        this.zIndexManager = zIndexManager;
        this.bar = bar || element.querySelector('.draggable-bar');

        // Initial positions
        this.startX = 0;
        this.startY = 0;
        this.dragging = false;

        this.bar.addEventListener('mousedown', this.initDrag);
    }

    updatePosition = (clientX, clientY) => {
        let top = clientY - this.startY;
        let left = clientX - this.startX;

        // Constraints for top and left positions:
        top = Math.max(0, Math.min(top, window.innerHeight - this.element.offsetHeight));
        left = Math.max(0, Math.min(left, window.innerWidth - this.element.offsetWidth));

        this.element.style.position = 'absolute';
        this.element.style.top = `${top}px`;
        this.element.style.left = `${left}px`;
    }

    initDrag = (e) => {
        e.preventDefault();
        this.dragging = true;

        const { clientX, clientY, target } = e;
        if (target === this.bar) {
            const { left, top } = this.element.getBoundingClientRect();
            this.startX = clientX - left;
            this.startY = clientY - top;
        }

        document.addEventListener('mousemove', this.doDrag);
        document.addEventListener('mouseup', this.stopDrag);
    }

    doDrag = (e) => {
        if (!this.dragging) return;
        this.updatePosition(e.clientX, e.clientY);
    }

    stopDrag = () => {
        this.dragging = false;
        document.removeEventListener('mousemove', this.doDrag);
        document.removeEventListener('mouseup', this.stopDrag);
    }
}
